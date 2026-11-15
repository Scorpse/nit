use super::*;

use crate::backend::tests::backend_with_saved_login;
use store::SecretStore;

fn repo(name: &str) -> RepoRef {
    RepoRef::parse_bitbucket(name).unwrap()
}

fn denied(name: &str) -> (RepoRef, String) {
    (repo(name), "access denied".into())
}

/// An inbox showing demo data, wired to a backend with saved (fake) credentials.
fn signed_in_app() -> (App, std::sync::Arc<store::MemorySecrets>) {
    let (b, secrets) = backend_with_saved_login();
    let mut app = App::demo(None, false);
    app.status_label = None;
    app.login.email = "ana@example.com".into();
    app.login.has_saved = true;
    app.backend = Some(b);
    (app, secrets)
}

#[test]
fn a_review_action_updates_the_inbox_copy_and_the_cache() {
    use prcore::model::{ReviewState, Reviewer};
    let (mut app, _) = signed_in_app();
    let pr = app.data.prs[0].clone();
    let store = app.backend.as_ref().unwrap().store_handle();
    store
        .lock()
        .unwrap()
        .replace_prs(&pr.repo, std::slice::from_ref(&pr))
        .unwrap();
    let mine = vec![Reviewer {
        user: app.me.clone(),
        state: ReviewState::Approved,
    }];
    app.dirty = false;
    app.apply_reviewers(&pr.repo, pr.id, mine.clone());
    assert_eq!(app.data.prs[0].reviewers, mine, "the inbox row");
    assert!(app.dirty, "rows are rebuilt");
    app.backend.as_ref().unwrap().writer().flush();
    let cached = store.lock().unwrap().list_prs().unwrap();
    assert_eq!(cached[0].reviewers, mine, "a later snapshot keeps it");
}

#[test]
fn auth_rejected_keeps_credentials_prefills_email_and_explains() {
    let (mut app, secrets) = signed_in_app();
    app.sync.running = true;
    assert!(!app.apply_msg(Msg::AuthRejected));
    assert!(app.screen == Screen::Login);
    assert_eq!(app.login.email, "ana@example.com");
    assert_eq!(app.login.error.as_deref(), Some(AUTH_REJECTED));
    assert!(app.login.token.is_empty());
    assert!(app.login.has_saved && !app.login.busy && !app.sync_busy());
    let (ek, tk) = crate::accounts::bitbucket_keys();
    assert!(secrets.get(&tk).unwrap().is_some());
    assert!(secrets.get(&ek).unwrap().is_some());
}

#[test]
fn sign_out_clears_credentials_me_and_snapshot() {
    let (mut app, secrets) = signed_in_app();
    app.selected = Some((repo("acme/api"), 102));
    app.sign_out();
    assert!(app.screen == Screen::Login);
    assert!(app.data.prs.is_empty() && app.data.errors.is_empty());
    assert!(app.me.id.is_empty() && app.me.display_name.is_empty());
    assert!(app.selected.is_none() && app.detail.is_none());
    assert!(app.login.email.is_empty() && !app.login.has_saved);
    assert_eq!(app.login.error, None);
    let (ek, tk) = crate::accounts::bitbucket_keys();
    assert_eq!(secrets.get(&tk).unwrap(), None);
    assert_eq!(secrets.get(&ek).unwrap(), None);
    assert_eq!(app.backend.as_ref().unwrap().stored_me(), None);
}

#[test]
fn snapshot_not_built_on_ui_thread() {
    let (mut app, _) = signed_in_app();
    let store = app.backend.as_ref().unwrap().store_handle();
    crate::backend::tests::populate(&store.lock().unwrap(), 2, 3);
    // a sync writer holds the store mutex
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = store.lock().unwrap();
        locked_tx.send(()).unwrap();
        // bounded: a drain that waits for the store fails the timing check, never hangs
        let _ = release_rx.recv_timeout(Duration::from_secs(2));
    });
    locked_rx.recv().unwrap();
    let ctx = egui::Context::default();
    // the end of a sync: the inbox must be refreshed now
    let tx = app.backend.as_ref().unwrap().test_sender();
    tx.send(Msg::Sync(SyncEvent::Finished)).unwrap();
    let t = Instant::now();
    app.drain_backend(&ctx);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "the refresh waited for the store"
    );
    assert!(!app.refresh_pending, "the refresh was asked for");
    let _ = release_tx.send(());
    holder.join().unwrap();
    // the snapshot arrives as a message and is swapped in
    let t = Instant::now();
    while app.data.prs.len() != 6 && t.elapsed() < Duration::from_secs(10) {
        app.drain_backend(&ctx);
        std::thread::yield_now();
    }
    assert_eq!(app.data.prs.len(), 6);
    assert_eq!(app.data.repo_count, 2);
}

#[test]
fn unreadable_cache_shows_a_banner_and_keeps_the_inbox() {
    let (mut app, _) = signed_in_app();
    let shown = app.data.prs.len();
    assert!(shown > 0);
    app.apply_msg(Msg::Snapshot(Err(CACHE_READ_FAILED.into())));
    assert_eq!(app.sync.note.as_deref(), Some(CACHE_READ_FAILED));
    assert_eq!(app.data.prs.len(), shown, "not an empty inbox");
    app.apply_msg(Msg::Snapshot(Ok(Snapshot::default())));
    assert_eq!(app.sync.note, None);
    assert!(app.data.prs.is_empty());
}

#[test]
fn requested_elsewhere_hint_is_dismissible_and_selects_nothing() {
    let (mut app, _) = signed_in_app();
    let gh = AccountId("acc-gh".into());
    let r = |n: &str| RepoRef::parse("github.com", n).unwrap();
    let before = (app.data.repo_count, app.screen);
    assert_eq!(app.elsewhere_text(), None);
    app.apply_msg(Msg::RequestedElsewhere {
        account: gh.clone(),
        repos: vec![(r("o/a"), 2)],
    });
    let hint = app.elsewhere_text().unwrap();
    assert!(hint.contains("o/a"), "{hint}");
    assert!(
        (app.data.repo_count, app.screen) == before,
        "a hint only: no selection, no screen change"
    );
    app.elsewhere_dismissed = Some(hint);
    assert_eq!(app.elsewhere_text(), None);
    // the account's next answer replaces its last one, and a new hint shows again
    app.apply_msg(Msg::RequestedElsewhere {
        account: gh.clone(),
        repos: vec![(r("o/a"), 2), (r("o/b"), 1)],
    });
    assert!(app.elsewhere_text().unwrap().contains("o/b"));
    app.apply_msg(Msg::RequestedElsewhere {
        account: gh,
        repos: vec![],
    });
    assert_eq!(app.elsewhere_text(), None, "nothing outside the selection");
}

#[test]
fn another_accounts_snapshot_is_never_shown() {
    let (mut app, _) = signed_in_app();
    let me = app.me.id.clone();
    let shown = app.data.prs.len();
    let of = |account: &str, repo_count| Snapshot {
        repo_count,
        account: Some(account.into()),
        ..Snapshot::default()
    };
    app.apply_msg(Msg::Snapshot(Ok(of("{someone-else}", 7))));
    assert_eq!(app.data.prs.len(), shown, "dropped");
    app.apply_msg(Msg::Snapshot(Ok(of(&me, 7))));
    assert_eq!(app.data.repo_count, 7);
    // a sign-in as someone else empties the screen at once
    app.apply_msg(Msg::Snapshot(Ok(of(&me, 3))));
    app.selected = Some((repo("acme/api"), 102));
    app.apply_msg(Msg::Connected(UserRef {
        id: "{bo}".into(),
        display_name: "Bo".into(),
        login: None,
    }));
    assert_eq!(app.data, Snapshot::default());
    assert!(app.selected.is_none());
    app.apply_msg(Msg::Snapshot(Ok(of(&me, 3))));
    assert_eq!(app.data.repo_count, 0, "the old account's late snapshot");
    // signed out: no account's data is shown
    app.sign_out();
    app.apply_msg(Msg::Snapshot(Ok(of("{bo}", 5))));
    assert_eq!(app.data.repo_count, 0);
}

#[test]
fn only_the_newest_queued_snapshot_is_applied() {
    let (mut app, _) = signed_in_app();
    let b = app.backend.as_ref().unwrap();
    let tx = b.test_sender();
    for n in 1..=3 {
        let snap = Snapshot {
            repo_count: n,
            ..Snapshot::default()
        };
        tx.send(Msg::Snapshot(Ok(snap))).unwrap();
    }
    app.drain_backend(&egui::Context::default());
    assert_eq!(app.data.repo_count, 3);
}

#[test]
fn refresh_and_retry_never_double_start() {
    let mut app = App::blank(Screen::Inbox);
    assert!(app.begin_sync());
    assert!(!app.begin_sync(), "second start while connecting");
    app.apply_msg(Msg::Connected(UserRef {
        id: "{u}".into(),
        display_name: "U".into(),
        login: None,
    }));
    assert!(!app.begin_sync(), "between me() and the first sync event");
    app.apply_msg(Msg::Sync(SyncEvent::Started));
    assert!(!app.begin_sync(), "while syncing");
    assert!(app.apply_msg(Msg::Sync(SyncEvent::Finished)));
    assert!(app.begin_sync(), "after the sync finished");
    app.apply_msg(Msg::Failed("Could not connect".into()));
    assert!(app.begin_sync(), "after a failed connect");

    let mut login = App::blank(Screen::Login);
    assert!(login.begin_sync() && login.login.busy);
    assert!(!login.begin_sync());
    login.apply_msg(Msg::AuthRejected);
    assert!(login.begin_sync());
}

fn found(selected: usize) -> Result<crate::backend::Discovered, String> {
    Ok(crate::backend::Discovered { repos: 4, selected })
}

#[test]
fn a_first_sign_in_without_chosen_repos_opens_the_picker() {
    let (mut app, _) = signed_in_app();
    app.apply_msg(Msg::Discovery {
        first: true,
        result: found(0),
    });
    assert!(app.screen == Screen::Repos);
    assert!(app.picker_from == Screen::Inbox);
    // not when something is chosen, not for a later discovery
    for (first, selected) in [(true, 2), (false, 0)] {
        let (mut app, _) = signed_in_app();
        app.apply_msg(Msg::Discovery {
            first,
            result: found(selected),
        });
        assert!(app.screen == Screen::Inbox);
    }
    // a failed listing is said in the status line
    let (mut app, _) = signed_in_app();
    let e = "Could not list your repositories (network error)".to_string();
    app.apply_msg(Msg::Discovery {
        first: true,
        result: Err(e.clone()),
    });
    assert!(app.screen == Screen::Inbox);
    assert_eq!(app.sync.note, Some(e));
}

#[test]
fn a_saved_choice_returns_to_the_inbox_and_a_failed_one_stays() {
    let mut app = App::demo(None, false);
    app.open_picker(Screen::Settings);
    app.picker = Some(RepoPicker::loading());
    app.apply_msg(Msg::SelectionSaved(Err("Could not save".into())));
    assert!(app.screen == Screen::Repos);
    assert_eq!(
        app.picker.as_ref().unwrap().error.as_deref(),
        Some("Could not save")
    );
    assert!(app.apply_msg(Msg::SelectionSaved(Ok(()))), "refresh now");
    assert!(app.screen == Screen::Inbox && app.picker.is_none());
    assert!(app.refresh_pending);
}

#[test]
fn a_reconnect_does_not_leave_the_picker() {
    let (mut app, _) = signed_in_app();
    app.open_picker(Screen::Inbox);
    let me = app.me.clone();
    app.apply_msg(Msg::Connected(me.clone()));
    assert!(app.screen == Screen::Repos);
    let mut login = App::blank(Screen::Login);
    login.apply_msg(Msg::Connected(me));
    assert!(login.screen == Screen::Inbox);
}

#[test]
fn the_upgrade_notice_names_the_count() {
    assert!(preselected_notice(1).contains("the 1 repository you"));
    assert!(preselected_notice(3).contains("the 3 repositories you"));
}

#[test]
fn retry_without_backend_starts_nothing() {
    let mut app = App::blank(Screen::Inbox);
    app.retry();
    assert!(!app.sync_busy());
}

#[test]
fn repo_failures_are_counted_as_denied() {
    let mut app = App::blank(Screen::Inbox);
    app.apply_msg(Msg::Sync(SyncEvent::Started));
    app.apply_msg(Msg::Sync(SyncEvent::Discovered { repos: 2 }));
    for name in ["w/a", "w/b"] {
        app.apply_msg(Msg::Sync(SyncEvent::RepoFailed {
            repo: repo(name),
            error: "access denied".into(),
        }));
    }
    app.apply_msg(Msg::Sync(SyncEvent::Finished));
    assert!(app.sync.finished);
    assert_eq!((app.sync.total, app.sync.denied), (2, 2));
}

#[test]
fn access_denied_hint_only_when_everything_was_denied() {
    assert!(access_denied_everywhere(
        2,
        2,
        &[denied("w/a"), denied("w/b")]
    ));
    assert!(!access_denied_everywhere(
        3,
        2,
        &[denied("w/a"), denied("w/b")]
    ));
    // No repos found because every workspace listing was denied.
    assert!(access_denied_everywhere(0, 0, &[denied("w/*")]));
    assert!(!access_denied_everywhere(
        0,
        0,
        &[denied("w/*"), (repo("v/*"), "network error".into())]
    ));
    assert!(!access_denied_everywhere(0, 0, &[]));
}

#[test]
fn stack_debug_line_shows_both_short_hashes() {
    let (_, prs, _) = crate::demo::demo_data();
    let find = |slug: &str, id: u64| {
        prs.iter()
            .find(|p| p.repo.slug == slug && p.id == id)
            .unwrap()
            .clone()
    };
    let (parent, mut child) = (find("billing", 201), find("billing", 202));
    assert_eq!(
        stack_debug_line(&child, Some(&parent)),
        "debug: destination.commit c201-ol | parent #201 source.commit c201-fo"
    );
    child.dest_commit = String::new();
    assert_eq!(
        stack_debug_line(&child, None),
        "debug: source.commit c202 | destination.commit unknown"
    );
}

#[test]
fn review_button_follows_fork_credentials_and_git() {
    use crate::settings_ui::{REVIEW_NEEDS_GIT, REVIEW_NEEDS_SIGN_IN};
    let select = |app: &mut App, slug: &str, id: u64| {
        app.selected = Some((repo(slug), id));
        app.dirty = true;
        app.recompute();
    };
    let mut demo = App::demo(None, false);
    demo.git_ok = Arc::new(OnceLock::from(true));
    assert_eq!(detail_review_button(&demo), None, "nothing selected");
    select(&mut demo, "acme/api", 102);
    assert_eq!(detail_review_button(&demo), Some(ReviewButton::Enabled));
    select(&mut demo, "acme/api", 110); // the fork
    assert_eq!(detail_review_button(&demo), Some(ReviewButton::Hidden));
    select(&mut demo, "acme/api", 102);
    demo.git_ok = Arc::new(OnceLock::new());
    assert_eq!(
        detail_review_button(&demo),
        Some(ReviewButton::Disabled(REVIEW_NEEDS_GIT)),
        "git check not finished or failed"
    );

    let (mut real, _) = signed_in_app();
    real.git_ok = Arc::new(OnceLock::from(true));
    select(&mut real, "acme/api", 102);
    assert_eq!(detail_review_button(&real), Some(ReviewButton::Enabled));
    real.login.has_saved = false;
    assert_eq!(
        detail_review_button(&real),
        Some(ReviewButton::Disabled(REVIEW_NEEDS_SIGN_IN))
    );
    // the same account connects again (another account would clear the selection)
    let me = real.me.clone();
    real.apply_msg(Msg::Connected(me));
    assert_eq!(detail_review_button(&real), Some(ReviewButton::Enabled));
}

#[test]
fn enter_opens_review_only_without_focus() {
    assert!(enter_opens_review(true, false, false));
    assert!(!enter_opens_review(false, false, false));
    assert!(
        !enter_opens_review(true, true, false),
        "search box had focus"
    );
    assert!(
        !enter_opens_review(true, false, true),
        "focus lost this frame"
    );
}

#[test]
fn detail_of_a_stack_member_carries_the_debug_line() {
    let mut app = App::demo(None, false);
    app.selected = Some((repo("acme/billing"), 202));
    app.recompute();
    let debug = app.detail.as_ref().and_then(|d| d.debug.clone());
    assert_eq!(
        debug.as_deref(),
        Some("debug: destination.commit c201-ol | parent #201 source.commit c201-fo")
    );
    app.selected = Some((repo("acme/billing"), 211));
    app.dirty = true;
    app.recompute();
    assert_eq!(app.detail.as_ref().unwrap().debug, None);
}

#[test]
fn unknown_me_falls_back_to_all() {
    assert_eq!(effective_view(false, View::NeedsMe), View::All);
    assert_eq!(effective_view(false, View::Mine), View::All);
    assert_eq!(effective_view(false, View::Stacked), View::Stacked);
    assert_eq!(effective_view(true, View::NeedsMe), View::NeedsMe);
}

#[test]
fn repaint_delay_never_underflows() {
    assert_eq!(
        repaint_delay(Duration::from_millis(100)),
        Duration::from_millis(400)
    );
    assert_eq!(repaint_delay(Duration::from_millis(500)), Duration::ZERO);
    assert_eq!(repaint_delay(Duration::from_secs(3)), Duration::ZERO);
}

#[test]
fn advice_text_names_pr_numbers_for_every_variant() {
    let cases = [
        (Advice::ReviewBottomUp, vec![]),
        (
            Advice::ParentChanged {
                parent: 41,
                child: 42,
            },
            vec!["#41", "#42"],
        ),
        (
            Advice::ParentMergedRetarget {
                parent: 43,
                child: 44,
            },
            vec!["#43", "#44"],
        ),
        (
            Advice::HoldChildApproval {
                parent: 45,
                child: 46,
            },
            vec!["#45", "#46"],
        ),
        (
            Advice::MergeOrderViolation {
                parent: 47,
                child: 48,
            },
            vec!["#47", "#48"],
        ),
        (Advice::ReviewablePerPr, vec![]),
    ];
    for (advice, numbers) in cases {
        let text = advice_text(&advice);
        assert!(!text.is_empty(), "empty text for {advice:?}");
        for n in numbers {
            assert!(text.contains(n), "{advice:?}: {text:?} lacks {n}");
        }
    }
}
