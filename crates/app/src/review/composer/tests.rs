use super::*;
use chrono::TimeZone;
use prcore::comments::build_threads;
use std::time::Duration;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn repo() -> RepoRef {
    RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "acme".into(),
        slug: "api".into(),
    }
}

fn row(kind: RowKind, old: Option<u32>, new: Option<u32>) -> Row {
    Row {
        kind,
        old_line: old,
        new_line: new,
        text: "x".into(),
    }
}

/// a.rs: ctx 10/10, del old 11, add new 11, ctx 12/12.
fn source(head: &str) -> RowSource {
    let head = head.to_string();
    Arc::new(move |path| {
        let rows = if path == "a.rs" {
            vec![
                row(RowKind::HunkHeader, None, None),
                row(RowKind::Context, Some(10), Some(10)),
                row(RowKind::Del, Some(11), None),
                row(RowKind::Add, None, Some(11)),
                row(RowKind::Context, Some(12), Some(12)),
            ]
        } else {
            vec![]
        };
        Ok(FileRows {
            repo: repo(),
            pr_id: 7,
            head: head.clone(),
            merge_base: MERGE_BASE.into(),
            rows: Arc::new(rows),
            binary: path.ends_with(".png"),
        })
    })
}

fn mem_store() -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
}

fn composer(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>) -> Composer {
    let mut c = Composer::new(repo(), 7, store);
    c.head = HEAD.into();
    c.inline_enabled = true;
    c.connect(poster.clone(), source(HEAD), None);
    c
}

fn write(c: &mut Composer, target: Target, text: &str) -> i64 {
    c.open_editor(target);
    c.editor.as_mut().unwrap().text = text.into();
    assert!(c.save_editor(), "saved");
    c.drafts.last().unwrap().id
}

fn inline(path: &str, side: Side, line: u32) -> Target {
    Target::Inline {
        path: path.into(),
        side,
        line,
        start: None,
    }
}

/// Runs a confirmed submit to the end.
fn submit(c: &mut Composer, comments: &mut Comments) -> PollOutcome {
    c.request_submit();
    assert!(c.confirm.is_some(), "confirmation opens");
    assert!(c.confirm_submit());
    wait(c, comments)
}

fn wait(c: &mut Composer, comments: &mut Comments) -> PollOutcome {
    let start = Instant::now();
    let mut all = PollOutcome::default();
    loop {
        let o = c.poll(comments);
        all.refresh |= o.refresh;
        all.stale |= o.stale;
        all.threads_changed |= o.threads_changed;
        if !c.running() && !c.any_resolving() {
            return all;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "batch hung");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn stored(store: &Arc<Mutex<Store>>) -> Vec<Draft> {
    store.lock().unwrap().list_drafts(&repo(), 7).unwrap()
}

fn texts(v: &[NewComment]) -> Vec<String> {
    v.iter()
        .map(|c| match c {
            NewComment::Pr { text }
            | NewComment::File { text, .. }
            | NewComment::Inline { text, .. }
            | NewComment::Reply { text, .. } => text.clone(),
        })
        .collect()
}

#[test]
fn submit_posts_in_order_and_deletes_on_success() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(&HEAD[..12]));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, Target::Pr, "one");
    write(&mut c, Target::File("a.rs".into()), "two");
    write(&mut c, inline("a.rs", Side::New, 11), "three");
    write(&mut c, inline("a.rs", Side::Old, 11), "four");
    assert_eq!(stored(&store).len(), 4);
    submit(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["one", "two", "three", "four"]
    );
    assert!(stored(&store).is_empty(), "deleted after each 2xx");
    assert!(c.drafts.is_empty());
    assert_eq!(comments.threads.len(), 4, "shown as threads at once");
    let a = comments.threads[2].root.anchor.as_ref().unwrap();
    assert_eq!((a.path.as_str(), a.to), ("a.rs", Some(11)));
}

#[test]
fn failed_post_keeps_draft_and_stops_batch_not_double_posts_on_retry() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, Target::Pr, "one");
    let two = write(&mut c, Target::Pr, "two");
    write(&mut c, Target::Pr, "three");
    poster
        .script
        .lock()
        .unwrap()
        .extend([None, Some(PostError::Network("network error".into()))]);
    submit(&mut c, &mut comments);
    assert_eq!(texts(&poster.attempts.lock().unwrap()), ["one", "two"]);
    let left: Vec<String> = stored(&store).into_iter().map(|d| d.text).collect();
    assert_eq!(left, ["two", "three"], "failed and unattempted drafts stay");
    assert!(matches!(c.state(two), DraftState::Failed(e) if e.contains("network")));
    // retry: only the drafts still present, each posted exactly once overall
    submit(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["one", "two", "three"]
    );
    assert!(stored(&store).is_empty() && c.drafts.is_empty());
    assert_eq!(comments.threads.len(), 3);
}

#[test]
fn batch_stops_after_first_error() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let first = write(&mut c, Target::Pr, "one");
    write(&mut c, Target::File("a.rs".into()), "two");
    write(
        &mut c,
        Target::Reply {
            thread: ThreadId::from(5),
        },
        "three",
    );
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Rejected("access denied".into())));
    submit(&mut c, &mut comments);
    assert_eq!(poster.attempts.lock().unwrap().len(), 1, "nothing after it");
    assert!(poster.posted.lock().unwrap().is_empty());
    assert_eq!(stored(&store).len(), 3);
    assert!(matches!(c.state(first), DraftState::Failed(e) if e.contains("access denied")));
    assert!(!c.running(), "Submit is enabled again");
}

const MERGE_BASE: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn inline_check_uses_the_forges_anchor_model_and_base() {
    // a forge anchoring to diff refs: same head but another (or an unknown) merge base
    // never posts, and the draft stays; the reviewed base posts
    for (base, posted) in [
        (Some("another-base"), false),
        (None, false),
        (Some(MERGE_BASE), true),
    ] {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        *poster.anchor.lock().unwrap() = AnchorModel::DiffRefs;
        *poster.server_base.lock().unwrap() = base.map(Into::into);
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, inline("a.rs", Side::New, 11), "on a line");
        submit(&mut c, &mut comments);
        if posted {
            assert_eq!(poster.posted.lock().unwrap().len(), 1);
            assert!(stored(&store).is_empty());
        } else {
            assert!(poster.attempts.lock().unwrap().is_empty(), "never posted");
            assert_eq!(c.state(id), DraftState::Blocked(InlineBlock::BaseMismatch));
            assert_eq!(stored(&store).len(), 1, "still a draft");
        }
    }
}

#[test]
fn every_post_carries_the_reviewed_revision() {
    // the provider gets exactly what was fetched: an inline draft its file's head and
    // merge base, a PR-level draft the review's head (no base needed)
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.anchor.lock().unwrap() = AnchorModel::DiffRefs;
    *poster.server_base.lock().unwrap() = Some(MERGE_BASE.into());
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, inline("a.rs", Side::New, 11), "on a line");
    write(&mut c, Target::Pr, "overall");
    submit(&mut c, &mut comments);
    assert_eq!(
        *poster.revisions.lock().unwrap(),
        vec![
            PostRevision {
                head: HEAD.into(),
                merge_base: MERGE_BASE.into()
            },
            PostRevision {
                head: HEAD.into(),
                merge_base: String::new()
            },
        ]
    );
}

#[test]
fn bitbucket_inline_check_ignores_the_base() {
    // Bitbucket (current diff): whatever base the server reports, the head decides
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    *poster.server_base.lock().unwrap() = Some("unrelated".into());
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, inline("a.rs", Side::New, 11), "on a line");
    submit(&mut c, &mut comments);
    assert_eq!(poster.posted.lock().unwrap().len(), 1);
}

#[test]
fn inline_blocked_when_head_changed_offers_file_level_only() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new("fedcba9876543210"));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write(&mut c, inline("a.rs", Side::New, 11), "on a line");
    let out = submit(&mut c, &mut comments);
    assert!(poster.attempts.lock().unwrap().is_empty(), "never posted");
    assert_eq!(c.state(id), DraftState::Blocked(InlineBlock::HeadMismatch));
    assert_eq!(stored(&store).len(), 1, "still a draft");
    assert!(out.stale && c.stale, "the screen learns the PR moved");
    // only an explicit click turns it into a file draft; still nothing is posted
    c.as_file_comment(id);
    let left = stored(&store);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].kind, DraftKind::File);
    assert_eq!(left[0].path.as_deref(), Some("a.rs"));
    assert_eq!(left[0].text, "(line 11) on a line");
    assert!(poster.attempts.lock().unwrap().is_empty());
    // the file draft posts fine while the PR is stale
    submit(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["(line 11) on a line"]
    );
}

#[test]
fn inline_blocked_for_line_not_in_diff() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let far = write(&mut c, inline("a.rs", Side::New, 500), "far away");
    let wrong_side = write(&mut c, inline("a.rs", Side::Old, 11), "old side ok");
    let added_on_old = write(&mut c, inline("a.rs", Side::New, 12), "ctx new 12");
    let bin = write(&mut c, inline("logo.png", Side::New, 1), "binary");
    submit(&mut c, &mut comments);
    let blocked = |id| c.state(id);
    assert_eq!(
        blocked(far),
        DraftState::Blocked(InlineBlock::LineNotInDiff)
    );
    assert_eq!(blocked(bin), DraftState::Blocked(InlineBlock::BinaryFile));
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["old side ok", "ctx new 12"],
        "a block is not a failure: the valid drafts still post"
    );
    let left: Vec<i64> = stored(&store).into_iter().map(|d| d.id).collect();
    assert_eq!(left, [far, bin]);
    assert!(!c
        .drafts
        .iter()
        .any(|d| d.id == wrong_side || d.id == added_on_old));
}

#[test]
fn drafts_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cache.sqlite");
    let poster = Arc::new(MemoryPoster::new(HEAD));
    {
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut c = composer(store, &poster);
        write(&mut c, inline("a.rs", Side::New, 11), "kept");
        write(
            &mut c,
            Target::Reply {
                thread: ThreadId::from(3),
            },
            "reply kept",
        );
    }
    let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
    let c = composer(store, &poster);
    assert_eq!(c.drafts.len(), 2);
    let d = &c.drafts[0];
    assert_eq!(
        (d.kind, d.path.as_deref(), d.side, d.line),
        (DraftKind::Inline, Some("a.rs"), Some(Side::New), Some(11))
    );
    assert_eq!(d.source_commit, HEAD, "the fetched head at draft time");
    assert_eq!(c.drafts[1].reply_to, Some(ThreadId::from(3)));
}

#[test]
fn empty_draft_not_saved() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    c.open_editor(Target::Pr);
    c.editor.as_mut().unwrap().text = "  \n\t ".into();
    assert!(!c.save_editor());
    assert!(c.editor.is_none() && c.drafts.is_empty());
    assert!(stored(&store).is_empty());
    // switching composers keeps typed text as a draft but drops empty ones
    c.open_editor(Target::Pr);
    c.open_editor(Target::File("a.rs".into()));
    assert!(stored(&store).is_empty());
    c.editor.as_mut().unwrap().text = "typed".into();
    c.open_editor(Target::Pr);
    assert_eq!(stored(&store).len(), 1);
    // emptying an existing draft does not change it
    let id = c.drafts[0].id;
    c.cancel_editor();
    c.edit_draft(id);
    c.editor.as_mut().unwrap().text.clear();
    assert!(!c.save_editor());
    assert_eq!(stored(&store)[0].text, "typed");
}

fn comment(id: u64, resolved: bool) -> Comment {
    let t = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    Comment {
        id: id.into(),
        parent_id: None,
        author: UserRef {
            id: "{a}".into(),
            display_name: "Ana".into(),
            login: None,
        },
        body: "b".into(),
        created_on: t,
        updated_on: t,
        deleted: false,
        resolved,
        anchor: None,
        author_is_bot: false,
        thread: Some(prcore::comments::ThreadId::from(id)),
        can_resolve: true,
    }
}

#[test]
fn resolve_calls_poster_and_updates_thread_with_undo() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    let mut comments = Comments::new();
    comments.set(build_threads(vec![comment(1, false), comment(2, false)]), 0);
    c.toggle_resolved(&mut comments, &1.into());
    assert!(comments.threads[0].root.resolved, "optimistic");
    assert!(c.is_resolving(&1.into()));
    c.toggle_resolved(&mut comments, &1.into()); // disabled while in flight
    wait(&mut c, &mut comments);
    assert_eq!(
        *poster.resolves.lock().unwrap(),
        [(ThreadId::from(1), true)]
    );
    assert!(comments.threads[0].root.resolved);
    assert!(c.toast.as_ref().is_some_and(|t| t.can_undo && t.resolved));
    c.undo(&mut comments);
    assert!(!comments.threads[0].root.resolved);
    wait(&mut c, &mut comments);
    assert_eq!(
        *poster.resolves.lock().unwrap(),
        [(ThreadId::from(1), true), (ThreadId::from(1), false)]
    );
    assert!(c.toast.as_ref().is_some_and(|t| !t.can_undo));
    // a refusal reverts the thread and says so
    poster
        .resolve_script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Rejected("access denied".into())));
    c.toggle_resolved(&mut comments, &2.into());
    assert!(comments.threads[1].root.resolved);
    wait(&mut c, &mut comments);
    assert!(!comments.threads[1].root.resolved, "reverted");
    assert!(c
        .error
        .as_deref()
        .is_some_and(|e| e.contains("access denied")));
    assert!(c.toast.is_none());
    // the toast goes after UNDO_FOR
    c.toggle_resolved(&mut comments, &1.into());
    wait(&mut c, &mut comments);
    c.toast.as_mut().unwrap().at = Instant::now() - UNDO_FOR;
    c.poll(&mut comments);
    assert!(c.toast.is_none());
}

/// A resolve whose answer was lost may have landed: no failure, the comments are
/// refetched and the server's state is adopted; it shows as done when it applied.
#[test]
fn resolve_with_unknown_outcome_refreshes_instead_of_failing() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    let mut comments = Comments::new();
    comments.set(build_threads(vec![comment(1, false), comment(2, false)]), 0);
    poster
        .resolve_script
        .lock()
        .unwrap()
        .extend([Some(PostError::Unconfirmed), Some(PostError::Unconfirmed)]);
    // it applied on the server
    c.toggle_resolved(&mut comments, &1.into());
    let out = wait(&mut c, &mut comments);
    assert!(out.refresh, "the comments are fetched again");
    assert!(c.error.is_none(), "not shown as a failure");
    assert!(c
        .notice
        .as_deref()
        .is_some_and(|n| n.starts_with("Not confirmed")));
    assert!(comments.threads[0].root.resolved, "not reverted meanwhile");
    let mut server = comment(1, true);
    server.body = "b".into();
    comments.set(build_threads(vec![server, comment(2, false)]), 0);
    c.resolves_checked(&comments);
    assert!(c.notice.is_none(), "the refresh shows it applied");
    assert!(c.error.is_none());
    // it did not apply: the server's state is shown, and said to be unconfirmed
    c.toggle_resolved(&mut comments, &2.into());
    let out = wait(&mut c, &mut comments);
    assert!(out.refresh);
    comments.set(build_threads(vec![comment(1, true), comment(2, false)]), 0);
    c.reapply_pending(&mut comments);
    c.resolves_checked(&comments);
    assert!(!comments.threads[1].root.resolved, "the server's state");
    assert!(c.error.is_none());
    let n = c.notice.clone().unwrap_or_default();
    assert!(
        n.contains("Not confirmed") && n.contains("refreshed"),
        "{n}"
    );
}

fn posting_ids(store: &Arc<Mutex<Store>>) -> Vec<i64> {
    store.lock().unwrap().posting_draft_ids(&repo(), 7).unwrap()
}

const UNKNOWN: DraftState = DraftState::Unconfirmed {
    checked: false,
    looks_posted: false,
};

#[test]
fn timeout_after_send_is_unconfirmed_and_excluded_from_postable() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write(&mut c, Target::Pr, "one");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Unconfirmed));
    submit(&mut c, &mut comments);
    assert_eq!(c.state(id), UNKNOWN);
    let d = c.draft(id).unwrap().clone();
    assert_eq!(c.state(id).note(&d).unwrap().0, UNCONFIRMED_TEXT);
    assert_eq!(
        UNCONFIRMED_TEXT,
        "Not confirmed: the connection failed after sending. Refresh comments to check \
         before posting again."
    );
    assert!(c.postable().is_empty(), "not offered again");
    assert_eq!(stored(&store).len(), 1, "kept as a draft");
    assert_eq!(posting_ids(&store), [id], "the marker stays");
    c.request_submit();
    assert!(c.confirm.is_none(), "nothing to submit");
    // editing it does not make it postable either
    c.edit_draft(id);
    c.editor.as_mut().unwrap().text = "one, edited".into();
    assert!(c.save_editor());
    assert_eq!(c.state(id), UNKNOWN);
    assert!(c.postable().is_empty());
    assert_eq!(poster.attempts.lock().unwrap().len(), 1);
}

#[test]
fn connect_failure_is_definite() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write(&mut c, Target::Pr, "one");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Network("network error".into())));
    submit(&mut c, &mut comments);
    let d = c.draft(id).unwrap().clone();
    assert!(c.state(id).note(&d).unwrap().0.starts_with("Not posted"));
    assert!(posting_ids(&store).is_empty(), "the marker is cleared");
    assert_eq!(c.postable(), [id], "can be submitted again");
}

#[test]
fn restart_with_posting_marker_is_unconfirmed() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let id = write(&mut composer(store.clone(), &poster), Target::Pr, "one");
    // a crash between "posting started" and the answer
    store.lock().unwrap().mark_draft_posting(id).unwrap();
    let c = composer(store.clone(), &poster);
    assert_eq!(c.state(id), UNKNOWN);
    assert!(c.postable().is_empty());
    // while this process is still posting it, it shows as posting
    let reg = InFlight::default();
    reg.lock().unwrap().insert(id);
    let other = Composer::with_in_flight(repo(), 7, store.clone(), reg);
    assert_eq!(other.state(id), DraftState::Posting);
}

#[test]
fn batch_stops_after_unconfirmed() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, Target::Pr, "one");
    let two = write(&mut c, Target::Pr, "two");
    let three = write(&mut c, Target::Pr, "three");
    poster
        .script
        .lock()
        .unwrap()
        .extend([None, Some(PostError::Unconfirmed)]);
    submit(&mut c, &mut comments);
    assert_eq!(texts(&poster.attempts.lock().unwrap()), ["one", "two"]);
    assert_eq!(c.state(two), UNKNOWN);
    assert_eq!(c.state(three), DraftState::Idle);
    assert_eq!(c.postable(), [three]);
}

#[test]
fn post_again_requires_explicit_rearm() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write(&mut c, Target::Pr, "same text");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Unconfirmed));
    submit(&mut c, &mut comments);
    assert!(!c.rearm(id), "not before the comments were checked");
    assert_eq!(c.state(id), UNKNOWN);
    // "Check comments": a refresh is asked for; its result is compared
    assert!(c.check_comments(id));
    let me = UserRef {
        id: "{me}".into(),
        display_name: "Me".into(),
        login: None,
    };
    let mut mine = comment(50, false);
    mine.author = me.clone();
    mine.body = "same text".into();
    let mut other = comment(51, false);
    other.body = "same text".into();
    comments.set(build_threads(vec![other.clone()]), 0);
    c.comments_checked(&comments, Some(&me));
    assert_eq!(
        c.state(id),
        DraftState::Unconfirmed {
            checked: true,
            looks_posted: false
        },
        "someone else's identical text is not mine"
    );
    assert!(c.check_comments(id));
    comments.set(build_threads(vec![other, mine]), 0);
    c.comments_checked(&comments, Some(&me));
    let st = c.state(id);
    assert_eq!(
        st,
        DraftState::Unconfirmed {
            checked: true,
            looks_posted: true
        }
    );
    let d = c.draft(id).unwrap().clone();
    assert!(st.note(&d).unwrap().0.contains(LOOKS_POSTED));
    assert!(c.postable().is_empty(), "still not postable: hint only");
    assert_eq!(stored(&store).len(), 1, "never deleted automatically");
    // "Post again anyway"
    assert!(c.rearm(id));
    assert_eq!(c.state(id), DraftState::Idle);
    assert!(posting_ids(&store).is_empty());
    assert_eq!(c.postable(), [id]);
    submit(&mut c, &mut comments);
    assert_eq!(poster.attempts.lock().unwrap().len(), 2);
    assert!(stored(&store).is_empty());
}

/// "Check comments" must not miss a comment that was posted: the text sent counts even
/// after the draft was edited, and line endings or trailing blanks the server normalised
/// do not hide it. A check that found nothing says so.
#[test]
fn check_comments_finds_the_sent_text_and_reports_a_miss() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write(&mut c, Target::Pr, "line one\r\nline two\n");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Unconfirmed));
    submit(&mut c, &mut comments);
    let me = UserRef {
        id: "{me}".into(),
        display_name: "Me".into(),
        login: None,
    };
    // nothing there yet: the note says the check found nothing
    assert!(c.check_comments(id));
    comments.set(build_threads(vec![]), 0);
    c.comments_checked(&comments, Some(&me));
    let d = c.draft(id).unwrap().clone();
    let note = c.state(id).note(&d).unwrap().0;
    assert_ne!(note, UNCONFIRMED_TEXT, "a finished check is reported");
    assert!(note.contains(NOT_FOUND), "{note}");
    // edited after the unknown outcome; the server kept the sent text, normalised
    c.edit_draft(id);
    c.editor.as_mut().unwrap().text = "something else".into();
    assert!(c.save_editor());
    let mut mine = comment(60, false);
    mine.author = me.clone();
    mine.body = "line one\nline two".into();
    assert!(c.check_comments(id));
    comments.set(build_threads(vec![mine]), 0);
    c.comments_checked(&comments, Some(&me));
    assert_eq!(
        c.state(id),
        DraftState::Unconfirmed {
            checked: true,
            looks_posted: true
        }
    );
    assert!(c.postable().is_empty(), "still a hint only");
}

fn write_typed(c: &mut Composer, target: Target, t: CommentType, text: &str) -> i64 {
    c.open_editor(target);
    let e = c.editor.as_mut().unwrap();
    e.text = text.into();
    e.comment_type = Some(t);
    assert!(c.save_editor(), "saved");
    c.drafts.last().unwrap().id
}

#[test]
fn typed_draft_is_posted_with_one_prefix_also_on_retry() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write_typed(&mut c, Target::Pr, CommentType::Nit, "rename this");
    write(&mut c, Target::Pr, "untyped");
    // the stored text has no prefix: it is added when posting only
    assert_eq!(stored(&store)[0].text, "rename this");
    assert_eq!(stored(&store)[0].comment_type, Some(CommentType::Nit));
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Network("network error".into())));
    submit(&mut c, &mut comments);
    assert!(matches!(c.state(id), DraftState::Failed(_)));
    assert_eq!(
        stored(&store)[0].text,
        "rename this",
        "unchanged by the try"
    );
    submit(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.attempts.lock().unwrap()),
        ["**nit:** rename this", "**nit:** rename this", "untyped"]
    );
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["**nit:** rename this", "untyped"]
    );
    assert_eq!(comments.threads[0].root.body, "**nit:** rename this");
}

#[test]
fn editing_keeps_or_changes_the_type() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let id = write_typed(&mut c, Target::Pr, CommentType::Question, "why?");
    c.edit_draft(id);
    assert_eq!(
        c.editor.as_ref().unwrap().comment_type,
        Some(CommentType::Question),
        "the editor opens with the draft's type"
    );
    assert_eq!(
        c.editor.as_ref().unwrap().text,
        "why?",
        "no prefix in the text"
    );
    // Tab in the composer: the next type
    let e = c.editor.as_mut().unwrap();
    e.comment_type = CommentType::cycle(e.comment_type, true);
    assert!(c.save_editor());
    assert_eq!(
        stored(&store)[0].comment_type,
        Some(CommentType::Suggestion)
    );
    c.edit_draft(id);
    c.editor.as_mut().unwrap().comment_type = None;
    assert!(c.save_editor());
    assert_eq!(stored(&store)[0].comment_type, None);
    // a new composer starts without a type
    c.open_editor(Target::File("a.rs".into()));
    assert_eq!(c.editor.as_ref().unwrap().comment_type, None);
}

/// "Check comments" compares with the exact text sent, prefix included: also after a
/// restart and after the draft's type was changed.
#[test]
fn check_comments_matches_the_prefixed_text_that_was_sent() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let id = write_typed(&mut c, Target::Pr, CommentType::Issue, "leaks the handle");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Unconfirmed));
    submit(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.attempts.lock().unwrap()),
        ["**issue:** leaks the handle"]
    );
    let me = UserRef {
        id: "{me}".into(),
        display_name: "Me".into(),
        login: None,
    };
    let mine = |id: u64, body: &str| {
        let mut m = comment(id, false);
        m.author = me.clone();
        m.body = body.into();
        m
    };
    // the bare text is not what was sent
    assert!(c.check_comments(id));
    comments.set(build_threads(vec![mine(70, "leaks the handle")]), 0);
    c.comments_checked(&comments, Some(&me));
    assert_eq!(
        c.state(id),
        DraftState::Unconfirmed {
            checked: true,
            looks_posted: false
        }
    );
    // the type was changed afterwards: what was sent still counts
    c.edit_draft(id);
    c.editor.as_mut().unwrap().comment_type = Some(CommentType::Nit);
    assert!(c.save_editor());
    assert!(c.check_comments(id));
    comments.set(
        build_threads(vec![mine(71, "**issue:** leaks the handle\n")]),
        0,
    );
    c.comments_checked(&comments, Some(&me));
    assert!(matches!(
        c.state(id),
        DraftState::Unconfirmed {
            looks_posted: true,
            ..
        }
    ));
    // after a restart the sent text is rebuilt from the stored draft, prefix included
    let mut again = composer(store.clone(), &poster);
    assert!(again.check_comments(id));
    comments.set(
        build_threads(vec![mine(72, "**nit:** leaks the handle")]),
        0,
    );
    again.comments_checked(&comments, Some(&me));
    assert!(matches!(
        again.state(id),
        DraftState::Unconfirmed {
            looks_posted: true,
            ..
        }
    ));
    assert!(again.postable().is_empty(), "a hint only");
}

#[test]
fn submit_requires_confirmation_flag() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    assert!(!c.confirm_submit(), "no confirmation open: nothing starts");
    write(&mut c, Target::Pr, "one");
    c.request_submit();
    assert_eq!(c.confirm.as_deref(), Some(&[c.drafts[0].id][..]));
    std::thread::sleep(Duration::from_millis(100));
    c.poll(&mut comments);
    assert!(
        poster.attempts.lock().unwrap().is_empty(),
        "no post before confirm"
    );
    c.cancel_submit();
    assert!(!c.confirm_submit());
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert_eq!(stored(&store).len(), 1);
    submit(&mut c, &mut comments);
    assert_eq!(poster.posted.lock().unwrap().len(), 1);
    // without a poster there is no confirmation at all
    let mut offline = Composer::new(repo(), 7, store);
    write(&mut offline, Target::Pr, "x");
    offline.request_submit();
    assert!(offline.confirm.is_none() && offline.error.is_some());
}

#[test]
fn posted_unreadable_deletes_draft_and_refreshes() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    write(&mut c, Target::Pr, "one");
    write(&mut c, Target::Pr, "two");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::PostedUnreadable));
    let out = submit(&mut c, &mut comments);
    assert!(out.refresh, "the posted comment is fetched");
    assert!(
        stored(&store).is_empty(),
        "treated as posted, batch went on"
    );
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "two"]);
    assert!(c.drafts.is_empty());
}

#[test]
fn stale_disables_inline_submit_only() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    let line = write(&mut c, inline("a.rs", Side::New, 11), "inline");
    let pr = write(&mut c, Target::Pr, "pr");
    let file = write(&mut c, Target::File("a.rs".into()), "file");
    let reply = write(
        &mut c,
        Target::Reply {
            thread: ThreadId::from(1),
        },
        "reply",
    );
    c.stale = true;
    assert_eq!(c.postable(), [pr, file, reply]);
    assert_eq!(c.held_inline(), 1);
    c.request_submit();
    assert!(!c.confirm.as_ref().unwrap().contains(&line));
    assert!(c.confirm_submit());
    wait(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["pr", "file", "reply"]
    );
    assert_eq!(
        stored(&store).into_iter().map(|d| d.id).collect::<Vec<_>>(),
        [line]
    );
    // with only inline drafts left, Submit has nothing to offer
    c.request_submit();
    assert!(c.confirm.is_none());
}

/// A composer as the app builds it without NIT_INLINE_EXPERIMENT: inline posting off.
fn composer_inline_off(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>) -> Composer {
    let mut c = composer(store, poster);
    c.inline_enabled = false;
    c
}

#[test]
fn inline_disabled_refuses_inline_drafts_but_posts_the_rest() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer_inline_off(store.clone(), &poster);
    let mut comments = Comments::new();
    let line = write(&mut c, inline("a.rs", Side::New, 11), "inline");
    let pr = write(&mut c, Target::Pr, "pr");
    let file = write(&mut c, Target::File("a.rs".into()), "file");
    let reply = write(&mut c, Target::Reply { thread: 1.into() }, "reply");
    assert_eq!(c.postable(), [pr, file, reply]);
    c.request_submit();
    let msg = c.error.clone().expect("the refusal is explained");
    assert!(msg.contains("1 inline draft"), "{msg}");
    assert!(
        msg.contains("file") && msg.contains("PR"),
        "offers conversion: {msg}"
    );
    assert_eq!(c.confirm.as_deref(), Some(&[pr, file, reply][..]));
    assert!(c.confirm_submit());
    wait(&mut c, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["pr", "file", "reply"]
    );
    let left = stored(&store);
    assert_eq!(left.len(), 1, "the inline draft stays saved");
    assert_eq!((left[0].id, left[0].kind), (line, DraftKind::Inline));
}

#[test]
fn inline_disabled_submit_with_only_inline_drafts_makes_no_network_call() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer_inline_off(store.clone(), &poster);
    let id = write(&mut c, inline("a.rs", Side::New, 11), "inline");
    c.request_submit();
    assert!(c.confirm.is_none(), "nothing to confirm");
    assert!(c.error.as_deref().is_some_and(|m| m.contains("inline")));
    // even a confirmation list forced by hand posts nothing
    c.confirm = Some(vec![id]);
    assert!(!c.confirm_submit());
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert!(poster.posted.lock().unwrap().is_empty());
    assert_eq!(stored(&store).len(), 1);
    assert_eq!(
        c.state(id),
        DraftState::Idle,
        "not silently marked or dropped"
    );
}

#[test]
fn inline_disabled_conversion_is_explicit_and_keeps_the_text() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer_inline_off(store.clone(), &poster);
    let a = write(&mut c, inline("a.rs", Side::New, 11), "one");
    let b = write(&mut c, inline("a.rs", Side::New, 12), "two");
    c.as_file_comment(a);
    c.as_pr_comment(b);
    let mut left: Vec<_> = stored(&store)
        .into_iter()
        .map(|d| (d.kind, d.path, d.text))
        .collect();
    left.sort_by_key(|d| format!("{:?}", d.0));
    assert_eq!(
        left,
        [
            (DraftKind::File, Some("a.rs".into()), "(line 11) one".into()),
            (DraftKind::Pr, None, "(a.rs line 12) two".into()),
        ]
    );
    assert!(poster.attempts.lock().unwrap().is_empty());
}

#[test]
fn inline_enabled_posts_inline_drafts() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    assert!(c.inline_enabled);
    let mut comments = Comments::new();
    write(&mut c, inline("a.rs", Side::New, 11), "inline");
    submit(&mut c, &mut comments);
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["inline"]);
    assert!(c.error.is_none());
    assert!(stored(&store).is_empty());
}

#[test]
fn inline_is_off_unless_the_experiment_flag_is_set() {
    // NIT_INLINE_EXPERIMENT is not set for the test run
    assert!(std::env::var_os("NIT_INLINE_EXPERIMENT").is_none());
    assert!(!inline_posting_enabled());
    let c = Composer::new(repo(), 7, mem_store());
    assert!(!c.inline_enabled);
}

#[test]
fn github_inline_is_gated_by_its_own_const_not_the_bitbucket_flag() {
    let mut c = Composer::new(repo(), 7, mem_store());
    // the Bitbucket gate is off in tests; GitHub follows only its own const
    assert!(!inline_posting_enabled());
    c.set_forge(Forge::GitHub);
    assert_eq!(c.inline_enabled(), providers::github::INLINE_ENABLED_GITHUB);
    assert!(c.inline_enabled());
    write(&mut c, inline("a.rs", Side::New, 11), "inline");
    // posted inline, not held
    assert_eq!(c.refused_inline(), 0);
    assert_eq!(c.postable().len(), 1);
    // and Bitbucket stays held
    c.set_forge(Forge::BitbucketCloud);
    assert_eq!(c.inline_enabled(), inline_posting_enabled());
    assert!(!c.inline_enabled());
}

/// GitLab has its own gate too; the Bitbucket experiment flag (off in tests) does not
/// decide it.
#[test]
fn gitlab_inline_is_gated_by_its_own_const() {
    let mut c = Composer::new(repo(), 7, mem_store());
    assert!(!inline_posting_enabled());
    c.set_forge(Forge::GitLab);
    assert_eq!(c.inline_enabled(), providers::gitlab::INLINE_ENABLED_GITLAB);
    assert!(c.inline_enabled());
    write(&mut c, inline("a.rs", Side::New, 11), "inline");
    assert_eq!(c.refused_inline(), 0);
    assert_eq!(c.postable().len(), 1);
}

/// The "experimental" badge names the Bitbucket environment variable only for
/// Bitbucket; the read-only tooltip names no forge.
#[test]
fn badge_texts_fit_every_forge() {
    assert!(inline_experiment_tip(Forge::BitbucketCloud).contains(INLINE_EXPERIMENT_VAR));
    for f in [Forge::GitHub, Forge::GitLab] {
        assert!(!inline_experiment_tip(f).contains("NIT_"), "{f:?}");
    }
    for name in ["GitHub", "GitLab", "Bitbucket"] {
        assert!(!READ_ONLY_TIP.contains(name), "{READ_ONLY_TIP}");
    }
    assert!(READ_ONLY_TIP.contains("read-only"));
}

#[test]
fn a_read_only_composer_opens_no_editor() {
    let mut c = Composer::new(repo(), 7, mem_store());
    c.read_only = true;
    for t in [
        Target::Pr,
        Target::Reply {
            thread: ThreadId("PRRT_x".into()),
        },
    ] {
        c.open_editor(t);
        assert!(c.editor.is_none());
    }
    assert!(c
        .error
        .as_deref()
        .is_some_and(|e| e.contains(READ_ONLY_TIP)));
    assert!(c.drafts.is_empty());
}

#[test]
fn draft_for_other_pr_not_shown() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut mine = composer(store.clone(), &poster);
    write(&mut mine, Target::Pr, "mine");
    let mut other = Composer::new(repo(), 8, store.clone());
    write(&mut other, Target::Pr, "other pr");
    let mut elsewhere = Composer::new(
        RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: "acme".into(),
            slug: "web".into(),
        },
        7,
        store.clone(),
    );
    write(&mut elsewhere, Target::Pr, "other repo");
    let again = composer(store, &poster);
    let shown: Vec<&str> = again.drafts.iter().map(|d| d.text.as_str()).collect();
    assert_eq!(shown, ["mine"]);
}

#[test]
fn plan_puts_drafts_under_their_rows_and_lists_the_rest() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    let rows: Vec<Row> = (*source(HEAD)("a.rs").unwrap().rows).clone();
    let threads = build_threads(vec![comment(1, false)]);
    let placement = Placement {
        inline: vec![(3, vec![0])],
        ..Default::default()
    };
    let reply = write(
        &mut c,
        Target::Reply {
            thread: ThreadId::from(1),
        },
        "r",
    );
    let on_add = write(&mut c, inline("a.rs", Side::New, 11), "x");
    let on_del = write(&mut c, inline("a.rs", Side::Old, 11), "y");
    let gone = write(&mut c, inline("a.rs", Side::New, 99), "z");
    c.open_editor(inline("a.rs", Side::Old, 10));
    let p = c.plan("a.rs", &rows, &placement, &threads);
    assert_eq!(
        p.rows.get(&3).unwrap(),
        &[Slot::Thread(0), Slot::Draft(reply), Slot::Draft(on_add)]
    );
    assert_eq!(p.rows.get(&2).unwrap(), &[Slot::Draft(on_del)]);
    assert_eq!(p.rows.get(&1).unwrap(), &[Slot::Editor]);
    assert_eq!(p.unplaced, [gone]);
    assert!(!p.editor_unplaced);
}

#[test]
fn abbreviated_server_head_matches_the_fetched_head() {
    assert_eq!(head_for_check(&HEAD[..12], HEAD), HEAD);
    assert_eq!(head_for_check("", HEAD), "");
    assert_eq!(head_for_check("0123", "0123"), "0123");
    assert_eq!(head_for_check("abc1234", HEAD), "abc1234");
}

// ---- safety: older versions, in-flight posts, tombstones ----

const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn composer_at(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>, head: &str) -> Composer {
    let mut c = Composer::new(repo(), 7, store);
    c.head = head.into();
    c.inline_enabled = true;
    c.connect(poster.clone(), source(head), None);
    c
}

fn until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn raw_inline(store: &Arc<Mutex<Store>>, source_commit: &str, text: &str) -> i64 {
    let d = Draft {
        id: 0,
        repo: repo(),
        pr_id: 7,
        kind: DraftKind::Inline,
        path: Some("a.rs".into()),
        line: Some(11),
        side: Some(Side::New),
        start: None,
        reply_to: None,
        text: text.into(),
        source_commit: source_commit.into(),
        comment_type: None,
    };
    store.lock().unwrap().save_draft(&d).unwrap()
}

#[test]
fn draft_from_older_head_blocked_after_reload() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let id = {
        let mut old = composer_at(store.clone(), &poster, OLD);
        write(&mut old, inline("a.rs", Side::New, 11), "on the old code")
    };
    // Reload: screen and server both at HEAD, and new line 11 exists there too
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    assert!(
        poster.attempts.lock().unwrap().is_empty(),
        "never on a guessed line"
    );
    assert_eq!(c.state(id), DraftState::Outdated);
    assert_eq!(stored(&store).len(), 1, "still a draft");
    c.as_file_comment(id);
    let left = stored(&store);
    assert_eq!(
        (left[0].kind, left[0].text.as_str()),
        (DraftKind::File, "(line 11) on the old code")
    );
    assert!(poster.attempts.lock().unwrap().is_empty());
}

#[test]
fn draft_from_older_head_blocked_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cache.sqlite");
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let id = {
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut old = composer_at(store, &poster, OLD);
        write(&mut old, inline("a.rs", Side::New, 11), "before the push")
    };
    let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert_eq!(c.state(id), DraftState::Outdated);
    // discarding is the other way out
    c.delete_draft(id);
    assert!(stored(&store).is_empty());
}

#[test]
fn stored_draft_with_empty_or_short_source_commit_is_blocked() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let empty = raw_inline(&store, "", "no head");
    let short = raw_inline(&store, &HEAD[..6], "six chars");
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    assert!(poster.attempts.lock().unwrap().is_empty(), "not trusted");
    assert_eq!(c.state(empty), DraftState::Outdated);
    assert_eq!(c.state(short), DraftState::Outdated);
}

#[test]
fn abbreviated_source_commit_still_matches() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    raw_inline(&store, &HEAD[..12], "short draft head");
    let mut c = composer(store.clone(), &poster);
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    // and a full draft head against an abbreviated screen head
    raw_inline(&store, HEAD, "full draft head");
    let mut short_screen = composer_at(store.clone(), &poster, &HEAD[..12]);
    submit(&mut short_screen, &mut comments);
    assert_eq!(
        texts(&poster.posted.lock().unwrap()),
        ["short draft head", "full draft head"]
    );
    assert!(stored(&store).is_empty());
}

#[test]
fn second_batch_skips_drafts_in_flight_across_composers() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    let mut a = composer(store.clone(), &poster);
    a.share_in_flight(reg.clone());
    let one = write(&mut a, Target::Pr, "one");
    let two = write(&mut a, Target::Pr, "two");
    *poster.hold.lock().unwrap() = Some("one".into());
    a.request_submit();
    assert!(a.confirm_submit());
    until("first post", || poster.attempts.lock().unwrap().len() == 1);
    let mut b = composer(store.clone(), &poster);
    b.share_in_flight(reg.clone());
    assert_eq!(
        b.state(one),
        DraftState::Posting,
        "shown as posting elsewhere"
    );
    assert_eq!(b.postable(), [two], "never in a second batch");
    let mut cb = Comments::new();
    submit(&mut b, &mut cb);
    poster.release();
    let mut ca = Comments::new();
    wait(&mut a, &mut ca);
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["two", "one"]);
    assert_eq!(poster.attempts.lock().unwrap().len(), 2);
    assert!(stored(&store).is_empty() && reg.lock().unwrap().is_empty());
}

#[test]
fn back_during_batch_stops_after_current_draft_and_keeps_rest() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    // Stop button: the screen stays
    let mut a = composer(store.clone(), &poster);
    a.share_in_flight(reg.clone());
    for t in ["one", "two", "three"] {
        write(&mut a, Target::Pr, t);
    }
    *poster.hold.lock().unwrap() = Some("one".into());
    a.request_submit();
    assert!(a.confirm_submit());
    until("first post", || poster.attempts.lock().unwrap().len() == 1);
    a.cancel_batch();
    poster.release();
    let mut comments = Comments::new();
    wait(&mut a, &mut comments);
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["one"]);
    let left: Vec<String> = stored(&store).into_iter().map(|d| d.text).collect();
    assert_eq!(left, ["two", "three"]);
    assert_eq!(a.drafts.len(), 2);
    // Back: the screen (and its composer) goes away mid-post
    *poster.hold.lock().unwrap() = Some("two".into());
    a.request_submit();
    assert!(a.confirm_submit());
    until("second post", || poster.attempts.lock().unwrap().len() == 2);
    drop(a);
    poster.release();
    until("worker done", || {
        reg.lock().unwrap().is_empty() && stored(&store).len() == 1
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "two"]);
    assert_eq!(stored(&store)[0].text, "three", "the rest stays a draft");
}

#[test]
fn reload_during_batch_never_double_posts() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    let mut a = composer(store.clone(), &poster);
    a.share_in_flight(reg.clone());
    let one = write(&mut a, Target::Pr, "one");
    let two = write(&mut a, Target::Pr, "two");
    *poster.hold.lock().unwrap() = Some("one".into());
    a.request_submit();
    assert!(a.confirm_submit());
    until("first post", || poster.attempts.lock().unwrap().len() == 1);
    drop(a); // Reload
    let mut b = composer(store.clone(), &poster);
    b.share_in_flight(reg.clone());
    assert_eq!(b.state(one), DraftState::Posting);
    b.request_submit();
    assert_eq!(b.confirm.as_deref(), Some(&[two][..]));
    assert!(b.confirm_submit());
    poster.release();
    let mut comments = Comments::new();
    wait(&mut b, &mut comments);
    until("old worker done", || reg.lock().unwrap().is_empty());
    let mut posted = texts(&poster.posted.lock().unwrap());
    posted.sort();
    assert_eq!(posted, ["one", "two"], "each exactly once");
    assert!(stored(&store).is_empty());
    b.request_submit();
    assert!(b.confirm.is_none(), "nothing left to post");
    assert_eq!(poster.attempts.lock().unwrap().len(), 2);
}

#[test]
fn posted_but_not_removed_is_never_posted_again_and_cleaned_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cache.sqlite");
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let id = {
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        // the batch's delete and its retry fail, and so does the cleanup on the next submit
        let mut c = Composer::with_failing_deletes(repo(), 7, store.clone(), 3);
        c.head = HEAD.into();
        c.connect(poster.clone(), source(HEAD), None);
        let id = write(&mut c, Target::Pr, "once");
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert_eq!(c.state(id), DraftState::PostedNotRemoved);
        assert_eq!(stored(&store).len(), 1, "the delete failed");
        assert_eq!(
            store.lock().unwrap().posted_draft_ids(&repo(), 7).unwrap(),
            [id]
        );
        c.request_submit();
        assert!(c.confirm.is_none(), "never offered again");
        id
    };
    // restart, and the cleanup delete fails once more: still not postable
    let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
    let mut c = Composer::with_failing_deletes(repo(), 7, store.clone(), 1);
    c.connect(poster.clone(), source(HEAD), None);
    assert_eq!(c.state(id), DraftState::PostedNotRemoved);
    assert!(c.postable().is_empty());
    drop(c);
    // the next open cleans the tombstone up
    let c = composer(store.clone(), &poster);
    assert!(c.drafts.is_empty() && stored(&store).is_empty());
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["once"]);
}

#[test]
fn draft_deleted_elsewhere_before_its_turn_is_skipped_as_gone() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut a = composer(store.clone(), &poster);
    write(&mut a, Target::Pr, "one");
    let two = write(&mut a, Target::Pr, "two");
    a.request_submit();
    assert_eq!(a.confirm.as_ref().map(Vec::len), Some(2));
    let mut b = composer(store.clone(), &poster);
    b.delete_draft(two);
    assert!(a.confirm_submit());
    let mut comments = Comments::new();
    wait(&mut a, &mut comments);
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["one"]);
    assert!(a.drafts.is_empty(), "the gone draft left the list");
}

#[test]
fn rows_of_another_review_abort_the_batch() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    // the service moved on (another head) since this screen loaded
    c.connect(poster.clone(), source(OLD), None);
    let line = write(&mut c, inline("a.rs", Side::New, 11), "x");
    write(&mut c, Target::Pr, "after it");
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert!(matches!(c.state(line), DraftState::Failed(e) if e.contains("reloaded")));
    // or another pull request altogether
    let other: RowSource = Arc::new(|_| {
        Ok(FileRows {
            repo: repo(),
            pr_id: 8,
            head: HEAD.into(),
            merge_base: String::new(),
            rows: Arc::new(vec![row(RowKind::Add, None, Some(11))]),
            binary: false,
        })
    });
    c.connect(poster.clone(), other, None);
    submit(&mut c, &mut comments);
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert!(matches!(c.state(line), DraftState::Failed(_)));
}

#[test]
fn undo_targets_the_toast_state_not_the_live_thread() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    let mut comments = Comments::new();
    comments.set(build_threads(vec![comment(1, false)]), 0);
    c.toggle_resolved(&mut comments, &1.into());
    wait(&mut c, &mut comments);
    // a refresh meanwhile shows it unresolved again (someone else)
    comments.set(build_threads(vec![comment(1, false)]), 0);
    c.undo(&mut comments);
    wait(&mut c, &mut comments);
    assert_eq!(
        *poster.resolves.lock().unwrap(),
        [(ThreadId::from(1), true), (ThreadId::from(1), false)]
    );
    assert!(!comments.threads[0].root.resolved);
}

#[test]
fn refresh_during_resolve_keeps_the_optimistic_state() {
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    let mut comments = Comments::new();
    comments.set(build_threads(vec![comment(1, false)]), 0);
    *poster.hold_resolve.lock().unwrap() = true;
    c.toggle_resolved(&mut comments, &1.into());
    comments.set(build_threads(vec![comment(1, false)]), 0);
    c.reapply_pending(&mut comments);
    assert!(
        comments.threads[0].root.resolved,
        "in flight: the click wins"
    );
    poster.release();
    wait(&mut c, &mut comments);
    assert!(comments.threads[0].root.resolved);
}

// ---- re-review: unrecorded posts, counts, registry on open ----

#[test]
fn unrecorded_post_is_never_submitted_again_in_this_process() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    // the server accepts, then neither the tombstone nor the delete can be written
    let mut c = Composer::with_failing_store(repo(), 7, store.clone(), reg.clone(), 99, 99);
    c.head = HEAD.into();
    c.connect(poster.clone(), source(HEAD), None);
    let id = write(&mut c, Target::Pr, "once");
    let mut comments = Comments::new();
    submit(&mut c, &mut comments);
    assert_eq!(c.state(id), DraftState::PostedNotRemoved);
    let err = c.error.clone().unwrap_or_default();
    assert!(err.contains("do not submit it again"), "{err}");
    c.request_submit();
    assert!(c.confirm.is_none());
    // another screen of this process does not offer it either
    let mut b = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
    b.connect(poster.clone(), source(HEAD), None);
    assert!(b.postable().is_empty());
    assert_eq!(b.state(id), DraftState::Posting);
    b.request_submit();
    assert!(b.confirm.is_none());
    assert_eq!(
        poster.attempts.lock().unwrap().len(),
        1,
        "posted exactly once"
    );
}

#[test]
fn progress_counts_only_drafts_that_will_post() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    let mut c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
    c.head = HEAD.into();
    c.connect(poster.clone(), source(HEAD), None);
    let blocked = write(&mut c, inline("a.rs", Side::New, 500), "blocked");
    let outdated = write(&mut c, inline("a.rs", Side::New, 11), "outdated");
    let one = write(&mut c, Target::Pr, "one");
    let two = write(&mut c, Target::Pr, "two");
    let three = write(&mut c, Target::Pr, "three");
    c.mark_blocked(blocked, InlineBlock::LineNotInDiff);
    c.states.insert(outdated, DraftState::Outdated);
    assert_eq!(
        c.postable(),
        [one, two, three],
        "blocked ones are not offered"
    );
    poster.hold_text(Some("one"));
    c.request_submit();
    assert_eq!(c.confirm.as_deref(), Some(&[one, two, three][..]));
    assert!(c.confirm_submit());
    let mut comments = Comments::new();
    until("first post", || poster.attempts.lock().unwrap().len() == 1);
    c.poll(&mut comments);
    assert_eq!(c.progress().as_deref(), Some("Posting 1 of 3..."));
    // another batch claims `two` meanwhile: it no longer counts here
    reg.lock().unwrap().insert(two);
    poster.hold_text(Some("three"));
    until("third post", || poster.attempts.lock().unwrap().len() == 2);
    std::thread::sleep(Duration::from_millis(50));
    c.poll(&mut comments);
    assert_eq!(c.progress().as_deref(), Some("Posting 2 of 2..."));
    poster.release();
    wait(&mut c, &mut comments);
    reg.lock().unwrap().remove(&two);
    assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "three"]);
    assert_eq!(
        c.state(blocked),
        DraftState::Blocked(InlineBlock::LineNotInDiff)
    );
}

#[test]
fn cleanup_on_first_open_respects_the_in_flight_registry() {
    let store = mem_store();
    let id = raw_inline(&store, HEAD, "being finished elsewhere");
    store.lock().unwrap().mark_draft_posted(id).unwrap();
    let reg = InFlight::default();
    reg.lock().unwrap().insert(id);
    let c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
    assert_eq!(
        stored(&store).len(),
        1,
        "its worker deletes it, not this open"
    );
    assert!(c.postable().is_empty());
    reg.lock().unwrap().clear();
    let c = Composer::with_in_flight(repo(), 7, store.clone(), reg);
    assert!(c.drafts.is_empty() && stored(&store).is_empty());
}

#[test]
fn drafts_in_flight_cannot_be_edited_deleted_or_converted() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let reg = InFlight::default();
    let mut c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
    c.head = HEAD.into();
    c.connect(poster.clone(), source(HEAD), None);
    let id = write(&mut c, inline("a.rs", Side::New, 11), "x");
    reg.lock().unwrap().insert(id);
    c.edit_draft(id);
    assert!(c.editor.is_none());
    c.delete_draft(id);
    c.as_file_comment(id);
    let left = stored(&store);
    assert_eq!((left.len(), left[0].kind), (1, DraftKind::Inline));
    assert!(c.error.is_some());
}

#[test]
fn stop_shows_stopping_until_the_batch_ends() {
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    write(&mut c, Target::Pr, "one");
    write(&mut c, Target::Pr, "two");
    poster.hold_text(Some("one"));
    c.request_submit();
    assert!(c.confirm_submit());
    until("first post", || poster.attempts.lock().unwrap().len() == 1);
    assert!(!c.stopping());
    c.cancel_batch();
    assert!(c.stopping());
    poster.release();
    let mut comments = Comments::new();
    wait(&mut c, &mut comments);
    assert!(!c.stopping());
    assert_eq!(stored(&store).len(), 1);
}

fn range(path: &str, start: (Side, u32), side: Side, line: u32) -> Target {
    Target::Inline {
        path: path.into(),
        side,
        line,
        start: Some(start),
    }
}

fn me() -> UserRef {
    UserRef {
        id: "{me}".into(),
        display_name: "Me".into(),
        login: None,
    }
}

/// A draft on a line range keeps its first line and side across a restart and is posted
/// as that range, never as its last line alone; a forge without ranges refuses one.
#[test]
fn multi_line_draft_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cache.sqlite");
    let poster = Arc::new(MemoryPoster::new(HEAD));
    // a.rs: from the deleted old line 11 to the context line 12 (new side)
    let target = range("a.rs", (Side::Old, 11), Side::New, 12);
    {
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut c = composer(store, &poster);
        c.forge = Forge::GitHub;
        write(&mut c, target.clone(), "these lines");
    }
    let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
    let mut c = composer(store.clone(), &poster);
    c.forge = Forge::GitHub;
    let d = c.drafts[0].clone();
    assert_eq!(d.start, Some((Side::Old, 11)));
    assert_eq!(Target::of(&d), Some(target.clone()));
    assert_eq!(describe(&d), "a.rs:11 (old side) to 12 (new side)");
    submit(&mut c, &mut Comments::new());
    assert_eq!(
        *poster.posted.lock().unwrap(),
        [NewComment::Inline {
            path: "a.rs".into(),
            side: Side::New,
            line: 12,
            start: Some((Side::Old, 11)),
            text: "these lines".into(),
        }]
    );
    assert!(stored(&store).is_empty());
    // Bitbucket takes no ranges: no composer opens on one
    let mut bb = composer(mem_store(), &poster);
    bb.open_editor(target);
    assert!(bb.editor.is_none() && bb.error.is_some());
}

/// GitLab saves a body without carriage returns and trailing blanks: "Check comments"
/// normalises both sides the same way before comparing.
#[test]
fn looks_posted_matches_gitlab_normalized_body() {
    assert_eq!(
        normalize_for_match(Forge::GitLab, "a\r\n  b \t\r\nc\rd\n\n"),
        "a\n  b\ncd"
    );
    assert_eq!(normalize_for_match(Forge::GitHub, "a  \nb\n\n"), "a\nb");
    assert_ne!(
        normalize_for_match(Forge::GitHub, "c\rd"),
        "cd",
        "only GitLab drops a lone carriage return"
    );
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    c.forge = Forge::GitLab;
    let mut comments = Comments::new();
    let id = write(&mut c, Target::Pr, "first  \r\nsecond\rhalf\t\r\n");
    poster
        .script
        .lock()
        .unwrap()
        .push_back(Some(PostError::Unconfirmed));
    submit(&mut c, &mut comments);
    let mut mine = comment(80, false);
    mine.author = me();
    mine.body = "first\nsecondhalf".into();
    assert!(c.check_comments(id));
    comments.set(build_threads(vec![mine]), 0);
    c.comments_checked(&comments, Some(&me()));
    assert_eq!(
        c.state(id),
        DraftState::Unconfirmed {
            checked: true,
            looks_posted: true
        }
    );
    assert!(c.postable().is_empty(), "a hint only");
}

/// New-side rows 10 to 12 of a.rs with their text.
fn texts_rows() -> Vec<Row> {
    let r = |kind, old, new, text: &str| Row {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
    };
    vec![
        r(RowKind::HunkHeader, None, None, "@@"),
        r(RowKind::Context, Some(10), Some(10), "ten"),
        r(RowKind::Del, Some(11), None, "gone"),
        r(RowKind::Add, None, Some(11), "eleven"),
        r(RowKind::Context, Some(12), Some(12), "twelve"),
    ]
}

#[test]
fn github_suggestion_block_format() {
    let lines = ["let a = 1;".to_string(), "let b = 2;".to_string()];
    assert_eq!(
        suggestion_block(SuggestionSyntax::GitHubFence, &lines, 12, (11, 12)),
        "```suggestion\nlet a = 1;\nlet b = 2;\n```"
    );
    // an empty suggestion removes the lines
    assert_eq!(
        suggestion_block(SuggestionSyntax::GitHubFence, &[], 12, (12, 12)),
        "```suggestion\n```"
    );
    // "Suggest change" on new lines 11-12 appends them to the text
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    c.forge = Forge::GitHub;
    c.open_editor(range("a.rs", (Side::New, 11), Side::New, 12));
    assert_eq!(
        c.editor.as_ref().unwrap().suggestions,
        Some(SuggestionSyntax::GitHubFence)
    );
    c.editor.as_mut().unwrap().text = "maybe:".into();
    c.insert_suggestion(&texts_rows());
    assert_eq!(
        c.editor.as_ref().unwrap().text,
        "maybe:\n```suggestion\neleven\ntwelve\n```"
    );
}

#[test]
fn gitlab_suggestion_offsets_relative_to_anchor() {
    let lines: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
    assert_eq!(
        suggestion_block(SuggestionSyntax::GitLabRange, &lines, 12, (10, 13)),
        "```suggestion:-2+1\na\nb\nc\nd\n```"
    );
    assert_eq!(
        suggestion_block(SuggestionSyntax::GitLabRange, &lines[..1], 12, (12, 12)),
        "```suggestion:-0+0\na\n```"
    );
    // the comment sits on its last line (12): the range starts one line above it
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(mem_store(), &poster);
    c.forge = Forge::GitLab;
    // GitLab's capabilities (ranges, suggestions) arrive with its provider: the composer
    // is set up as they will be
    c.editor = Some(Editor {
        target: range("a.rs", (Side::New, 11), Side::New, 12),
        text: String::new(),
        draft_id: None,
        comment_type: None,
        focus: false,
        suggestions: Some(SuggestionSyntax::GitLabRange),
    });
    c.insert_suggestion(&texts_rows());
    assert_eq!(
        c.editor.as_ref().unwrap().text,
        "```suggestion:-1+0\neleven\ntwelve\n```"
    );
    // Bitbucket has no suggestions: nothing is inserted
    let mut bb = composer(mem_store(), &poster);
    bb.open_editor(inline("a.rs", Side::New, 12));
    assert_eq!(bb.editor.as_ref().unwrap().suggestions, None);
    bb.insert_suggestion(&texts_rows());
    assert_eq!(bb.editor.as_ref().unwrap().text, "");
}

/// Decision D7: on GitLab a line starting with "/" and a letter would run a quick action.
#[test]
fn gitlab_slash_command_line_refused() {
    assert!(quick_action_guard(Forge::GitLab, "/approve").is_err());
    assert!(quick_action_guard(Forge::GitLab, "looks good\n/assign @me").is_err());
    assert!(quick_action_guard(Forge::GitLab, "  /merge").is_err());
    assert!(quick_action_guard(Forge::GitLab, "a/b and / alone\n/ x\n//c\n/1").is_ok());
    for f in [Forge::BitbucketCloud, Forge::GitHub] {
        assert!(quick_action_guard(f, "/approve").is_ok());
    }
    let store = mem_store();
    let poster = Arc::new(MemoryPoster::new(HEAD));
    let mut c = composer(store.clone(), &poster);
    c.forge = Forge::GitLab;
    // "Save draft" refuses it and keeps the text in the composer
    c.open_editor(Target::Pr);
    c.editor.as_mut().unwrap().text = "ok\n/merge".into();
    assert!(!c.save_editor());
    assert!(c.error.as_deref().unwrap().contains("quick action"));
    assert_eq!(c.editor.as_ref().unwrap().text, "ok\n/merge");
    assert!(stored(&store).is_empty());
    // switching composers keeps it as a draft, but a submit never sends it
    c.open_editor(Target::File("a.rs".into()));
    let id = stored(&store)[0].id;
    submit(&mut c, &mut Comments::new());
    assert!(poster.attempts.lock().unwrap().is_empty());
    assert!(matches!(c.state(id), DraftState::Failed(_)));
    assert_eq!(stored(&store).len(), 1, "kept");
}
