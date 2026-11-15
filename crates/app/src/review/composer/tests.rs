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

mod later;
