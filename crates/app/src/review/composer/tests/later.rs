//! More composer tests: inline gating, placement, restart and batch cancellation.

use super::*;

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
