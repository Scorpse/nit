use super::*;
use crate::demo::{review_fixture, review_fixture_with, DemoReview};
use prcore::diff::{FileStatus, RowKind};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);

fn store() -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
}

#[test]
fn fetch_spec_per_forge() {
    let mut pr = review_fixture().pr;
    pr.id = 7;
    assert_eq!(
        fetch_spec(&pr),
        FetchSpec::Branches {
            src: pr.source_branch.clone(),
            dst: pr.dest_branch.clone()
        }
    );
    for (forge, src_ref) in [
        (Forge::GitHub, "refs/pull/7/head"),
        (Forge::GitLab, "refs/merge-requests/7/head"),
    ] {
        pr.forge = forge;
        assert_eq!(
            fetch_spec(&pr),
            FetchSpec::PullRef {
                src_ref: src_ref.into(),
                dst: pr.dest_branch.clone()
            }
        );
    }
}

fn probe_returning(head: String) -> HeadProbe {
    Arc::new(move |_, _| Ok(head.clone()))
}

fn no_comments() -> CommentFetcher {
    Arc::new(|_, _| Ok(CommentList::default()))
}

fn local(f: &DemoReview) -> ReviewService {
    ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        probe_returning(f.head.clone()),
        no_comments(),
    )
}

/// Polls until `done` holds for the collected events; panics after `WAIT`.
fn collect_until(svc: &ReviewService, done: impl Fn(&[ReviewEvent]) -> bool) -> Vec<ReviewEvent> {
    let start = Instant::now();
    let mut all = Vec::new();
    loop {
        all.extend(svc.poll());
        if done(&all) {
            return all;
        }
        assert!(start.elapsed() < WAIT, "timed out; got {all:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn opened(evs: &[ReviewEvent]) -> Option<(PrRefs, Vec<DiffFile>)> {
    evs.iter().find_map(|e| match e {
        ReviewEvent::Opened { refs, files } => Some((refs.clone(), files.clone())),
        _ => None,
    })
}

fn failed(evs: &[ReviewEvent]) -> bool {
    evs.iter().any(|e| matches!(e, ReviewEvent::Failed(_)))
}

fn counted(evs: &[ReviewEvent]) -> bool {
    evs.iter()
        .any(|e| matches!(e, ReviewEvent::Counts { .. } | ReviewEvent::CountsFailed(_)))
}

/// Opens and collects events until the list and its counts (or a failure) are in.
fn open_events(svc: &ReviewService, f: &DemoReview) -> Vec<ReviewEvent> {
    svc.open(f.repo.clone(), f.pr.clone());
    collect_until(svc, |e| (opened(e).is_some() && counted(e)) || failed(e))
}

/// The opened PR's files with their counts filled in.
fn open_and_wait(svc: &ReviewService, f: &DemoReview) -> (PrRefs, Vec<DiffFile>) {
    let evs = open_events(svc, f);
    let (refs, mut files) = opened(&evs).unwrap_or_else(|| panic!("{evs:?}"));
    if let Some(c) = counts_in(&evs) {
        apply_counts(&mut files, &c);
    }
    (refs, files)
}

/// The opened PR's list alone (the counts may still be out).
fn open_listed(svc: &ReviewService, f: &DemoReview) -> (PrRefs, Vec<DiffFile>) {
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(svc, |e| opened(e).is_some() || failed(e));
    opened(&evs).unwrap_or_else(|| panic!("{evs:?}"))
}

fn patch_for(evs: &[ReviewEvent], p: &str) -> Option<(Arc<Vec<Row>>, bool)> {
    evs.iter().find_map(|e| match e {
        ReviewEvent::Patch {
            path, rows, plain, ..
        } if path == p => Some((rows.clone(), *plain)),
        _ => None,
    })
}

#[test]
fn a_cleartext_account_fetches_over_http_from_its_own_origin_only() {
    let r = RepoRef::parse("192.168.1.20:8080", "root/nit-live-test").unwrap();
    let clear = GitAuth::cleartext(gitlocal::GITLAB_GIT_USER, "t", &r.host).unwrap();
    assert_eq!(
        remote_url(https_url(&r), &clear),
        "http://192.168.1.20:8080/root/nit-live-test.git"
    );
    let tls = GitAuth::new(gitlocal::GITLAB_GIT_USER, "t", &r.host);
    assert_eq!(
        remote_url(https_url(&r), &tls),
        "https://192.168.1.20:8080/root/nit-live-test.git"
    );
    // only an https url is rewritten (tests hand file:// urls)
    assert_eq!(remote_url("file:///x".into(), &clear), "file:///x");
}

#[test]
fn https_url_has_no_userinfo() {
    let r = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "acme".into(),
        slug: "api".into(),
    };
    assert_eq!(https_url(&r), "https://bitbucket.org/acme/api.git");
}

#[test]
fn github_pr_opens_with_pull_ref_spec_on_its_own_host_and_forks_are_allowed() {
    let gh = RepoRef::parse("github.com", "octo-org/api").unwrap();
    assert_eq!(https_url(&gh), "https://github.com/octo-org/api.git");
    let ghes = RepoRef::parse("ghe.corp.example:8443", "o/r").unwrap();
    assert_eq!(https_url(&ghes), "https://ghe.corp.example:8443/o/r.git");
    let mut pr = crate::demo::demo_data().1.remove(0);
    pr.repo = gh.clone();
    pr.source_repo = RepoRef::parse("github.com", "forker/api").unwrap();
    pr.forge = Forge::GitHub;
    pr.id = 7;
    assert!(!refuses_fork(&pr), "a GitHub fork PR is reviewed");
    assert_eq!(
        fetch_spec(&pr),
        FetchSpec::PullRef {
            src_ref: "refs/pull/7/head".into(),
            dst: pr.dest_branch.clone()
        }
    );
    pr.forge = Forge::BitbucketCloud;
    assert!(refuses_fork(&pr), "Bitbucket forks stay unsupported");
}

/// GitLab deletes `refs/merge-requests/<iid>/head` 14 days after close or merge: such an
/// MR is fetched by its source branch instead. An open MR never falls back (its ref is
/// the reviewed truth), nor does a fork MR (its branch is in another project).
#[test]
fn merged_mr_without_ref_falls_back_to_branch() {
    let mut pr = crate::demo::demo_data().1.remove(0);
    pr.repo = RepoRef::parse("git.corp.example/gitlab", "grp/sub/proj").unwrap();
    pr.source_repo = pr.repo.clone();
    pr.forge = Forge::GitLab;
    pr.id = 7;
    pr.state = PrState::Merged;
    assert_eq!(
        https_url(&pr.repo),
        "https://git.corp.example/gitlab/grp/sub/proj.git"
    );
    let tried = std::cell::RefCell::new(Vec::new());
    let fetch = |spec: &FetchSpec| {
        tried.borrow_mut().push(spec.clone());
        match spec {
            FetchSpec::PullRef { .. } => Err("couldn't find remote ref"),
            FetchSpec::Branches { .. } => Ok("fetched"),
        }
    };
    assert_eq!(fetch_with_fallback(&pr, fetch), Ok("fetched"));
    assert_eq!(
        *tried.borrow(),
        vec![
            FetchSpec::PullRef {
                src_ref: "refs/merge-requests/7/head".into(),
                dst: pr.dest_branch.clone()
            },
            FetchSpec::Branches {
                src: pr.source_branch.clone(),
                dst: pr.dest_branch.clone()
            }
        ]
    );
    for (state, fork) in [(PrState::Open, false), (PrState::Merged, true)] {
        pr.state = state;
        if fork {
            pr.source_repo =
                RepoRef::parse("git.corp.example/gitlab", "unknown/project-9").unwrap();
        }
        tried.borrow_mut().clear();
        assert!(fetch_with_fallback(&pr, fetch).is_err());
        assert_eq!(tried.borrow().len(), 1, "no fallback");
    }
}

#[test]
fn open_emits_phases_then_opened_with_sorted_index() {
    let f = review_fixture();
    let svc = local(&f);
    let evs = open_events(&svc, &f);
    let pos = evs
        .iter()
        .position(|e| matches!(e, ReviewEvent::Opened { .. }))
        .unwrap_or_else(|| panic!("{evs:?}"));
    let phases: Vec<&str> = evs[..pos]
        .iter()
        .filter_map(|e| match e {
            ReviewEvent::Phase(p) => Some(p.as_str()),
            _ => None,
        })
        .collect();
    assert!(phases.contains(&"Fetching branches..."), "{phases:?}");
    let (refs, mut files) = opened(&evs).unwrap();
    let counts = counts_in(&evs).unwrap_or_else(|| panic!("{evs:?}"));
    apply_counts(&mut files, &counts);
    assert_eq!(refs.head, f.head);
    assert_eq!(refs.dest, f.pr.dest_commit);
    assert_ne!(
        refs.base, refs.dest,
        "base is the merge-base, not the dest tip"
    );
    assert_eq!(files.len(), crate::demo::DEMO_REVIEW_FILES);
    let paths: Vec<&str> = files.iter().map(|d| d.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    assert!(files.iter().any(|d| d.status == FileStatus::Renamed));
    assert!(files.iter().any(|d| d.binary));
}

#[test]
fn patch_request_returns_rows_and_caches() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    svc.request_patch(m.path.clone(), true);
    let evs = collect_until(&svc, |e| patch_for(e, &m.path).is_some());
    let (rows, plain) = patch_for(&evs, &m.path).unwrap();
    assert!(!plain);
    assert!(rows.iter().any(|r| r.kind == RowKind::Add));
    assert!(rows.iter().any(|r| r.kind == RowKind::Del));
    assert!(svc.is_cached(&m.path));
    // a cached patch is served without waiting for a worker
    svc.request_patch(m.path.clone(), false);
    let again = svc.poll();
    let (rows2, _) = patch_for(&again, &m.path).unwrap_or_else(|| panic!("{again:?}"));
    assert!(Arc::ptr_eq(&rows, &rows2));
}

#[test]
fn prefetches_neighbours_of_requested_file() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0);
    let (_, files) = open_and_wait(&svc, &f);
    svc.request_patch(files[20].path.clone(), true);
    let start = Instant::now();
    let want = &files[20 - PREFETCH_RADIUS..=20 + PREFETCH_RADIUS];
    while !want.iter().all(|d| svc.is_cached(&d.path)) {
        assert!(start.elapsed() < WAIT, "neighbours not prefetched");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !svc.is_cached(&files[0].path),
        "only neighbours are prefetched"
    );
    let evs = svc.poll();
    let patches = evs
        .iter()
        .filter(|e| matches!(e, ReviewEvent::Patch { .. }))
        .count();
    assert_eq!(patches, 1, "prefetch fills the cache silently: {evs:?}");
}

#[test]
fn failed_fetch_emits_failed_without_token() {
    let dl = tempfile::tempdir().unwrap();
    let st = store();
    st.lock()
        .unwrap()
        .save_settings(&store::Settings {
            clone_root: None,
            download_dir: dl.path().to_path_buf(),
        })
        .unwrap();
    let auth = GitAuth::bitbucket("s3cr3tTOKEN");
    let svc = ReviewService::with_url_for(
        st,
        Some(auth),
        egui::Context::default(),
        probe_returning("x".into()),
        no_comments(),
        Arc::new(|_| "https://ana:s3cr3tTOKEN@bitbucket.org/acme/api.git".into()),
    );
    let f = review_fixture_with(1);
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, failed);
    assert!(opened(&evs).is_none());
    let shown = format!("{evs:?}");
    assert!(!shown.contains("s3cr3tTOKEN"), "{shown}");
    assert!(!shown.contains("ana@example.com"), "{shown}");
}

#[test]
fn open_without_credentials_fails_cleanly() {
    let svc = ReviewService::new(
        store(),
        None,
        egui::Context::default(),
        probe_returning("x".into()),
        no_comments(),
    );
    let f = review_fixture_with(1);
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, failed);
    assert!(opened(&evs).is_none(), "{evs:?}");
}

#[test]
fn close_cancels_pending_patches() {
    let f = review_fixture_with(200);
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    for d in &files {
        svc.request_patch(d.path.clone(), false);
    }
    svc.close();
    // jobs already running finish (slowly under load); nothing queued starts afterwards
    let start = Instant::now();
    let mut settled = svc.patches_built();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = svc.patches_built();
        if now == settled {
            break;
        }
        settled = now;
        assert!(start.elapsed() < WAIT, "workers kept going after close");
    }
    assert!(
        settled < files.len() as u64,
        "pending jobs were not cancelled"
    );
    let evs = svc.poll();
    assert!(evs.is_empty(), "{evs:?}");
    assert!(files.iter().all(|d| !svc.is_cached(&d.path)));
}

#[test]
fn no_events_after_close() {
    let f = review_fixture();
    let svc = local(&f);
    svc.open(f.repo.clone(), f.pr.clone());
    // a real changed file: had the open not been cancelled, it would be built
    svc.request_patch("src/m00/file_0004.rs".into(), true);
    svc.close();
    std::thread::sleep(Duration::from_millis(1500));
    let evs = svc.poll();
    assert!(evs.is_empty(), "{evs:?}");
    assert!(
        !svc.is_open(),
        "the cancelled load must not install its session"
    );
    assert_eq!(svc.patches_built(), 0, "queued work must never start");
}

#[test]
fn stale_when_inbox_source_commit_is_older_than_fetched_head() {
    let f = review_fixture_with(2);
    let svc = local(&f); // probe agrees with the fetched head
    let mut pr = f.pr.clone();
    pr.source_commit = f.git(&["rev-parse", "main~1"]); // older commit
    svc.open(f.repo.clone(), pr);
    collect_until(&svc, |e| e.iter().any(|x| matches!(x, ReviewEvent::Stale)));

    // empty inbox commit: no Stale
    let mut pr = f.pr.clone();
    pr.source_commit = String::new();
    svc.open(f.repo.clone(), pr);
    let mut evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    std::thread::sleep(Duration::from_millis(300));
    evs.extend(svc.poll());
    assert!(
        !evs.iter().any(|e| matches!(e, ReviewEvent::Stale)),
        "{evs:?}"
    );
}

#[test]
fn panicking_patch_build_fails_that_file_and_keeps_workers() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0); // every file goes through the (panicking) per-file build
    let (_, files) = open_and_wait(&svc, &f);
    let workers = svc.live_workers();
    assert!(workers >= 1);
    let bad: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(
        files[..5].iter().map(|d| d.path.clone()).collect(),
    ));
    let b = bad.clone();
    svc.set_patch_hook(Arc::new(move |p| {
        if b.lock().unwrap().contains(p) {
            panic!("injected panic for {p}");
        }
    }));
    for d in &files[..5] {
        svc.request_patch(d.path.clone(), true);
        collect_until(&svc, |e| {
            e.iter()
                .any(|x| matches!(x, ReviewEvent::PatchFailed { path, .. } if *path == d.path))
        });
    }
    assert_eq!(svc.live_workers(), workers, "a panic killed a worker");
    // other paths are still served, by every worker (not as priority requests: only
    // the newest few of those are kept)
    let others = &files[20..20 + 2 * workers];
    for d in others {
        svc.request_patch(d.path.clone(), false);
    }
    collect_until(&svc, |e| {
        others.iter().all(|d| patch_for(e, &d.path).is_some())
    });
    // the same path works once it stops panicking (in_flight was cleared)
    bad.lock().unwrap().clear();
    svc.request_patch(files[0].path.clone(), true);
    collect_until(&svc, |e| patch_for(e, &files[0].path).is_some());
}

#[test]
fn rapid_reopens_never_load_concurrently() {
    let f = review_fixture();
    let svc = local(&f);
    for _ in 0..5 {
        svc.open(f.repo.clone(), f.pr.clone());
    }
    let mut evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    std::thread::sleep(Duration::from_millis(1000));
    evs.extend(svc.poll());
    let opens = evs
        .iter()
        .filter(|e| matches!(e, ReviewEvent::Opened { .. }))
        .count();
    assert_eq!(opens, 1, "{evs:?}");
    assert!(!failed(&evs), "{evs:?}");
    assert_eq!(svc.max_concurrent_loads(), 1);
}

#[test]
fn reopen_cancels_previous() {
    let f = review_fixture();
    f.git(&["branch", "feature/two", "main"]);
    f.git(&["checkout", "-q", "feature/two"]);
    std::fs::write(f.repo_path.join("only.txt"), "one file\n").unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "second pr"]);
    let two_head = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    let mut pr2 = f.pr.clone();
    pr2.id = 901;
    pr2.source_branch = "feature/two".into();
    pr2.source_commit = two_head.clone();
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, id| {
            Ok(if id == 901 {
                two_head.clone()
            } else {
                "?".into()
            })
        }),
        no_comments(),
    );
    svc.open(f.repo.clone(), f.pr.clone());
    svc.open(f.repo.clone(), pr2);
    let mut evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    std::thread::sleep(Duration::from_millis(1000));
    evs.extend(svc.poll());
    let opens: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            ReviewEvent::Opened { files, .. } => Some(files.len()),
            _ => None,
        })
        .collect();
    assert_eq!(opens, vec![1], "{evs:?}");
    assert!(
        !evs.iter().any(|e| matches!(e, ReviewEvent::Stale)),
        "{evs:?}"
    );
}

#[test]
fn poll_is_nonblocking() {
    let f = review_fixture();
    let svc = local(&f);
    let t = Instant::now();
    assert!(svc.poll().is_empty());
    svc.open(f.repo.clone(), f.pr.clone());
    let _ = svc.poll();
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    open_and_wait_drain(&svc);
}

fn open_and_wait_drain(svc: &ReviewService) {
    collect_until(svc, |e| opened(e).is_some() || failed(e));
}

#[test]
fn row_source_builds_uncached_patch_and_flags_binary() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0);
    let rows = svc.row_source();
    assert!(rows("a.rs").is_err(), "not open yet");
    let (_, files) = open_and_wait(&svc, &f);
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    assert!(!svc.is_cached(&m.path));
    let got = rows(&m.path).unwrap();
    assert_eq!(got.head, f.head);
    assert!(!got.binary && got.rows.iter().any(|r| r.kind == RowKind::Add));
    assert!(svc.is_cached(&m.path), "built once, then cached");
    let bin = files.iter().find(|d| d.binary).unwrap();
    assert!(rows(&bin.path).unwrap().binary);
    let outside = rows("not/in/diff.rs").unwrap();
    assert!(outside.rows.is_empty() && !outside.binary);
}

#[test]
fn stale_when_pr_head_differs() {
    let f = review_fixture_with(2);
    let moved = Arc::new(Mutex::new(f.head.clone()));
    let m = moved.clone();
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, _| Ok(m.lock().unwrap().clone())),
        no_comments(),
    );
    open_and_wait(&svc, &f);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!svc.poll().iter().any(|e| matches!(e, ReviewEvent::Stale)));
    // short hashes (as Bitbucket returns them) match by prefix
    *moved.lock().unwrap() = f.head[..12].to_string();
    svc.recheck();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!svc.poll().iter().any(|e| matches!(e, ReviewEvent::Stale)));
    *moved.lock().unwrap() = "0123456789ab".into();
    svc.recheck();
    collect_until(&svc, |e| e.iter().any(|x| matches!(x, ReviewEvent::Stale)));

    // also detected right at open
    let svc2 = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        probe_returning("fedcba9876543210".into()),
        no_comments(),
    );
    svc2.open(f.repo.clone(), f.pr.clone());
    collect_until(&svc2, |e| e.iter().any(|x| matches!(x, ReviewEvent::Stale)));
}

#[test]
fn recheck_is_coalesced() {
    let f = review_fixture_with(2);
    let calls = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c, sl, head) = (calls.clone(), slow.clone(), f.head.clone());
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            if sl.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(400));
            }
            Ok(head.clone())
        }),
        no_comments(),
    );
    open_and_wait(&svc, &f);
    std::thread::sleep(Duration::from_millis(200));
    let at_open = calls.load(Ordering::SeqCst);
    slow.store(true, Ordering::SeqCst);
    // focus changes and timer ticks while one recheck is still out: one probe only
    let _ = svc.recheck();
    for _ in 0..5 {
        let _ = svc.recheck();
    }
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(calls.load(Ordering::SeqCst), at_open + 1);
    // once it is back, the next one runs
    let _ = svc.recheck();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(calls.load(Ordering::SeqCst), at_open + 2);
}

#[test]
fn recheck_emits_stale_when_head_moves() {
    let f = review_fixture_with(2);
    let remote = Arc::new(Mutex::new(f.head.clone()));
    let r = remote.clone();
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, _| Ok(r.lock().unwrap().clone())),
        no_comments(),
    );
    open_and_wait(&svc, &f);
    // someone pushed while the review was open
    *remote.lock().unwrap() = "0123456789abcdef".into();
    let _ = svc.recheck();
    collect_until(&svc, |e| e.iter().any(|x| matches!(x, ReviewEvent::Stale)));
}

#[test]
fn large_file_over_20k_changed_lines_marked_plain() {
    let f = review_fixture_with(2);
    f.git(&["checkout", "-q", "feature/demo"]);
    let big: String = (0..20_001).map(|i| format!("line {i}\n")).collect();
    std::fs::write(f.repo_path.join("big.txt"), big).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "big"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        probe_returning(head),
        no_comments(),
    );
    let (_, files) = open_and_wait(&svc, &f);
    let small = files.iter().find(|d| d.path != "big.txt").unwrap();
    svc.request_patch("big.txt".into(), true);
    svc.request_patch(small.path.clone(), true);
    let evs = collect_until(&svc, |e| {
        patch_for(e, "big.txt").is_some() && patch_for(e, &small.path).is_some()
    });
    let (rows, plain) = patch_for(&evs, "big.txt").unwrap();
    assert!(plain);
    assert_eq!(
        rows.iter().filter(|r| r.kind == RowKind::Add).count(),
        20_001
    );
    assert!(!patch_for(&evs, &small.path).unwrap().1);
}

#[test]
fn patch_for_unknown_path_emits_failed_for_that_file_only() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    svc.request_patch("no/such/file.rs".into(), true);
    svc.request_patch(files[0].path.clone(), true);
    let evs = collect_until(&svc, |e| {
        patch_for(e, &files[0].path).is_some()
            && e.iter()
                .any(|x| matches!(x, ReviewEvent::PatchFailed { .. }))
    });
    assert!(!failed(&evs), "{evs:?}");
    assert!(evs.iter().any(|e| matches!(
        e,
        ReviewEvent::PatchFailed { path, .. } if path == "no/such/file.rs"
    )));
}

/// The colours of `p` once every row arrived: one whole-file event as sent, or the
/// steps merged.
fn highlighted_for(evs: &[ReviewEvent], p: &str) -> Option<Arc<Spans>> {
    let mut merged: Vec<Option<RowSpans>> = Vec::new();
    for e in evs {
        let ReviewEvent::Highlighted {
            path,
            rows,
            range,
            spans,
        } = e
        else {
            continue;
        };
        if path != p {
            continue;
        }
        if *range == (0..*rows) {
            return Some(spans.clone());
        }
        merged.resize(*rows, None);
        for (i, s) in range.clone().zip(spans.iter()) {
            merged[i] = Some(s.clone());
        }
        if merged.iter().all(Option::is_some) {
            return Some(Arc::new(merged.into_iter().flatten().collect()));
        }
    }
    None
}

fn any_highlight_for(evs: &[ReviewEvent], p: &str) -> bool {
    evs.iter()
        .any(|e| matches!(e, ReviewEvent::Highlighted { path, .. } if path == p))
}

#[test]
fn highlighted_follows_patch_and_is_resent_from_memory() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    svc.request_patch(m.path.clone(), true);
    let evs = collect_until(&svc, |e| highlighted_for(e, &m.path).is_some());
    let (rows, _) = patch_for(&evs, &m.path).unwrap();
    let pos = |f: fn(&ReviewEvent) -> bool| evs.iter().position(f).unwrap();
    assert!(
        pos(|e| matches!(e, ReviewEvent::Patch { .. }))
            < pos(|e| matches!(e, ReviewEvent::Highlighted { .. }))
    );
    let spans = highlighted_for(&evs, &m.path).unwrap();
    assert_eq!(spans.len(), rows.len());
    assert!(spans.iter().any(|s| !s.is_empty()));
    std::thread::sleep(Duration::from_millis(200)); // the job has ended and remembers
    let started = Arc::new(Mutex::new(Vec::<String>::new()));
    let st = started.clone();
    svc.set_highlight_hook(Arc::new(move |p| st.lock().unwrap().push(p.to_string())));
    // back to the file: patch from cache and the colours at once, no new parse
    svc.request_patch(m.path.clone(), true);
    let again = svc.poll();
    assert!(patch_for(&again, &m.path).is_some());
    let remembered = highlighted_for(&again, &m.path).unwrap_or_else(|| panic!("{again:?}"));
    assert_eq!(*remembered, *spans);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !started.lock().unwrap().contains(&m.path),
        "no new job for the remembered file"
    );
}

#[test]
fn plain_file_gets_no_highlight_event() {
    let f = review_fixture_with(2);
    f.git(&["checkout", "-q", "feature/demo"]);
    let big: String = (0..20_001).map(|i| format!("fn f{i}() {{}}\n")).collect();
    std::fs::write(f.repo_path.join("big.rs"), big).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "big"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        probe_returning(head),
        no_comments(),
    );
    let (_, files) = open_and_wait(&svc, &f);
    let small = files
        .iter()
        .find(|d| d.path != "big.rs" && !d.binary)
        .unwrap();
    svc.request_patch("big.rs".into(), true);
    svc.request_patch(small.path.clone(), true);
    // the small file's colours arrive after its patch; by then big.rs would have had its
    let mut evs = collect_until(&svc, |e| highlighted_for(e, &small.path).is_some());
    std::thread::sleep(Duration::from_millis(300));
    evs.extend(svc.poll());
    assert!(
        patch_for(&evs, "big.rs").is_some_and(|(_, plain)| plain),
        "{evs:?}"
    );
    assert!(highlighted_for(&evs, "big.rs").is_none());
}

#[test]
fn highlighted_event_dropped_after_close() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    // hold the highlight job until the service was closed
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g = gate.clone();
    svc.set_highlight_hook(Arc::new(move |_| {
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    svc.request_patch(m.path.clone(), true);
    collect_until(&svc, |e| patch_for(e, &m.path).is_some());
    svc.close();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    std::thread::sleep(Duration::from_millis(500));
    let evs = svc.poll();
    assert!(evs.is_empty(), "{evs:?}");
    // a reopen must not see the old generation's colours either
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, |e| opened(e).is_some());
    assert!(highlighted_for(&evs, &m.path).is_none());
}

#[test]
fn panic_in_highlight_does_not_kill_worker() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let workers = svc.live_workers();
    let reviewable: Vec<&DiffFile> = files
        .iter()
        .filter(|d| d.status == FileStatus::Modified && !d.binary)
        .collect();
    let bad = reviewable[0].path.clone();
    let b = bad.clone();
    svc.set_highlight_hook(Arc::new(move |p| {
        if p == b {
            panic!("injected highlight panic for {p}");
        }
    }));
    svc.request_patch(bad.clone(), true);
    collect_until(&svc, |e| patch_for(e, &bad).is_some());
    std::thread::sleep(Duration::from_millis(300));
    assert!(svc
        .poll()
        .iter()
        .all(|e| !matches!(e, ReviewEvent::Highlighted { path, .. } if *path == bad)));
    assert_eq!(svc.live_workers(), workers, "a panic killed a worker");
    // other files are still patched and highlighted
    let good = reviewable[1].path.clone();
    svc.request_patch(good.clone(), true);
    collect_until(&svc, |e| highlighted_for(e, &good).is_some());
    // and the failed file is retried once it stops panicking
    *svc.shared.hl_hook.lock().unwrap() = None;
    svc.request_patch(bad.clone(), true);
    collect_until(&svc, |e| highlighted_for(e, &bad).is_some());
}

fn phases_before_end(evs: &[ReviewEvent]) -> Vec<String> {
    evs.iter()
        .take_while(|e| !matches!(e, ReviewEvent::Opened { .. } | ReviewEvent::Failed(_)))
        .filter_map(|e| match e {
            ReviewEvent::Phase(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// A remote service whose download dir is `dl` and whose remote refuses connections.
fn unreachable_remote(dl: &Path) -> ReviewService {
    let st = store();
    st.lock()
        .unwrap()
        .save_settings(&store::Settings {
            clone_root: None,
            download_dir: dl.to_path_buf(),
        })
        .unwrap();
    ReviewService::with_url_for(
        st,
        Some(GitAuth::bitbucket("tok")),
        egui::Context::default(),
        probe_returning("x".into()),
        no_comments(),
        Arc::new(|r: &RepoRef| format!("https://127.0.0.1:9/{}/{}.git", r.namespace, r.slug)),
    )
}

#[test]
fn local_open_phases_are_in_order() {
    let f = review_fixture_with(2);
    let svc = local(&f);
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    assert_eq!(
        phases_before_end(&evs),
        ["Locating repository...", FETCH_PHASE, LIST_PHASE],
        "{evs:?}"
    );
}

#[test]
fn first_remote_open_announces_the_clone() {
    let dl = tempfile::tempdir().unwrap();
    let svc = unreachable_remote(dl.path());
    let f = review_fixture_with(1);
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, failed);
    assert_eq!(
        phases_before_end(&evs),
        ["Locating repository...", CLONE_PHASE],
        "{evs:?}"
    );
    assert!(CLONE_PHASE.contains("first time"));
}

#[test]
fn remote_open_with_a_mirror_skips_the_clone_phase() {
    let dl = tempfile::tempdir().unwrap();
    let f = review_fixture_with(1);
    let mirror = dl
        .path()
        .join(&f.repo.namespace)
        .join(format!("{}.git", f.repo.slug));
    std::fs::create_dir_all(&mirror).unwrap();
    Git::locate()
        .unwrap()
        .run(&mirror, &["init", "-q", "--bare"], None)
        .unwrap();
    let svc = unreachable_remote(dl.path());
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, failed);
    assert_eq!(
        phases_before_end(&evs),
        ["Locating repository...", FETCH_PHASE],
        "{evs:?}"
    );
}

#[test]
fn clone_progress_shows_phase_and_percent() {
    let p = GitProgress {
        phase: "Receiving objects".into(),
        percent: 45,
    };
    assert_eq!(
        clone_progress_phase(&p),
        "Cloning the repository for the first time: receiving objects 45%"
    );
}

#[test]
fn listing_phase_says_when_contents_are_downloaded() {
    assert_eq!(files_phase(RepoKind::Mirror), MIRROR_FILES_PHASE);
    assert!(MIRROR_FILES_PHASE.contains("downloading file contents"));
    assert_eq!(files_phase(RepoKind::Clone), LIST_PHASE);
}

#[test]
fn loads_of_one_repository_serialize_across_services() {
    let f = review_fixture_with(2);
    let a = local(&f);
    let b = local(&f);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = channel::<&'static str>();
    let (g, ta) = (gate.clone(), Mutex::new(tx.clone()));
    a.set_load_hook(Arc::new(move |_| {
        ta.lock().unwrap().send("a").unwrap();
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    let tb = Mutex::new(tx);
    b.set_load_hook(Arc::new(move |_| tb.lock().unwrap().send("b").unwrap()));
    a.open(f.repo.clone(), f.pr.clone());
    assert_eq!(rx.recv_timeout(WAIT), Ok("a"));
    // a's load holds the process-wide lock of this repository
    let key = a.shared.load_key(&f.repo);
    assert_eq!(key, b.shared.load_key(&f.repo));
    assert!(repo_load_lock(&key).try_lock().is_err());
    b.open(f.repo.clone(), f.pr.clone());
    let start = Instant::now();
    while b.shared.load_waiters.load(Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < WAIT, "b never reached the lock");
        std::thread::yield_now();
    }
    assert!(rx.try_recv().is_err(), "b loaded while a was loading");
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert_eq!(rx.recv_timeout(WAIT), Ok("b"));
    collect_until(&a, |e| opened(e).is_some());
    collect_until(&b, |e| opened(e).is_some());
}

#[test]
fn patch_event_carries_its_precomputed_layout() {
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    svc.request_patch(m.path.clone(), true);
    let evs = collect_until(&svc, |e| patch_for(e, &m.path).is_some());
    let (rows, layout) = evs
        .iter()
        .find_map(|e| match e {
            ReviewEvent::Patch {
                path, rows, layout, ..
            } if *path == m.path => Some((rows.clone(), layout.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(*layout, PatchLayout::compute(&rows, false));
    assert!(layout.max_cols > 0);
    // served again from the cache with the same layout, nothing recomputed
    svc.request_patch(m.path.clone(), true);
    let again = svc.poll();
    let cached = again
        .iter()
        .find_map(|e| match e {
            ReviewEvent::Patch { layout, .. } => Some(layout.clone()),
            _ => None,
        })
        .unwrap();
    assert!(Arc::ptr_eq(&layout, &cached));
}

#[test]
fn held_jk_keeps_only_the_newest_priority_requests() {
    let mut st = State::new();
    st.enqueue("prefetch.rs".into(), false, false);
    st.enqueue("wanted.rs".into(), true, false);
    for i in 0..6 {
        st.enqueue(format!("p{i}.rs"), true, true);
    }
    let paths: Vec<&str> = st.queue.iter().map(|j| j.path.as_str()).collect();
    assert_eq!(
        paths,
        ["p5.rs", "p4.rs", "p3.rs", "prefetch.rs", "wanted.rs"]
    );
    let mut queued: Vec<&str> = st.queued.iter().map(|s| s.as_str()).collect();
    queued.sort();
    assert_eq!(
        queued,
        ["p3.rs", "p4.rs", "p5.rs", "prefetch.rs", "wanted.rs"]
    );
    // re-requesting a dropped file queues it again, at the front
    st.enqueue("p0.rs".into(), true, true);
    assert_eq!(st.queue[0].path, "p0.rs");
    assert_eq!(
        st.queue.iter().filter(|j| j.priority).count(),
        KEEP_PRIORITY
    );
}

#[test]
fn patch_cache_budget_is_256_mib() {
    assert_eq!(PATCH_CACHE_BYTES, 256 * 1024 * 1024);
}

#[test]
fn patch_is_served_while_a_highlight_blocks() {
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return; // a single worker does both jobs
    }
    let f = review_fixture();
    let svc = local(&f);
    let (_, files) = open_and_wait(&svc, &f);
    let reviewable: Vec<&DiffFile> = files
        .iter()
        .filter(|d| d.status == FileStatus::Modified && !d.binary)
        .collect();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g = gate.clone();
    svc.set_highlight_hook(Arc::new(move |_| {
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    // several files whose highlights block every highlight-capable worker
    for d in &reviewable[..6] {
        svc.request_patch(d.path.clone(), false);
    }
    collect_until(&svc, |e| {
        reviewable[..6]
            .iter()
            .all(|d| patch_for(e, &d.path).is_some())
    });
    let other = reviewable[7].path.clone();
    svc.request_patch(other.clone(), true);
    collect_until(&svc, |e| patch_for(e, &other).is_some());
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn two_workers() -> bool {
    std::thread::available_parallelism().map_or(1, |n| n.get()) >= 2
}

/// A gate the batch hook waits on for `which` batches; `entered` reports each entry.
fn gated_batches(
    svc: &ReviewService,
    which: &'static str,
) -> (Arc<(Mutex<bool>, Condvar)>, Receiver<()>) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = channel();
    let (g, tx) = (gate.clone(), Mutex::new(tx));
    svc.set_batch_hook(Arc::new(move |kind| {
        if kind != which {
            return;
        }
        let _ = tx.lock().unwrap().send(());
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    (gate, rx)
}

fn release(gate: &(Mutex<bool>, Condvar)) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

#[test]
fn neighbours_are_prefetched_with_one_git_process() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0);
    let git = Git::locate().unwrap();
    svc.use_git(git.clone());
    let (_, files) = open_and_wait(&svc, &f);
    let before = git.spawn_count();
    svc.request_patch(files[20].path.clone(), true);
    let want = &files[20 - PREFETCH_RADIUS..=20 + PREFETCH_RADIUS];
    wait_for("neighbours", || want.iter().all(|d| svc.is_cached(&d.path)));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        git.spawn_count() - before,
        2,
        "one process for the file, one for its {} neighbours",
        want.len() - 1
    );
    assert_eq!(svc.patches_built(), 1, "neighbours came from the batch");
    // the neighbours' rows are exactly the per-file rows
    let sess = svc.shared.lock().session.clone().unwrap();
    for d in want {
        let (rows, _) = svc.shared.lock().cache.get(&d.path).unwrap();
        let (per, _, _) = build_patch(&svc.shared, &sess, &d.path).unwrap();
        assert_eq!(*rows, *per, "{}", d.path);
    }
}

#[test]
fn small_pr_is_prefetched_whole_with_one_git_process() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let git = Git::locate().unwrap();
    svc.use_git(git.clone());
    let (_, files) = open_and_wait(&svc, &f);
    wait_for("whole PR", || files.iter().all(|d| svc.is_cached(&d.path)));
    std::thread::sleep(Duration::from_millis(300));
    // refs (1), merge-base (1), file list (2), commit log (1), then every patch of the PR (1)
    assert_eq!(git.spawn_count(), 6);
    assert_eq!(svc.patches_built(), 0);
    assert!(svc.batches_run() >= 1);
    // browsing now needs no git at all
    for d in &files[..5] {
        svc.request_patch(d.path.clone(), true);
    }
    let evs = collect_until(&svc, |e| {
        files[..5].iter().all(|d| patch_for(e, &d.path).is_some())
    });
    assert!(!evs
        .iter()
        .any(|e| matches!(e, ReviewEvent::PatchFailed { .. })));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(git.spawn_count(), 6);
}

#[test]
fn file_on_screen_is_served_before_the_whole_pr_batch() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let (gate, entered) = gated_batches(&svc, "whole");
    let (_, files) = open_and_wait(&svc, &f);
    entered.recv_timeout(WAIT).expect("whole-PR batch started");
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && !d.binary)
        .unwrap();
    svc.request_patch(m.path.clone(), true);
    // served by its own process while the big batch is still held
    collect_until(&svc, |e| patch_for(e, &m.path).is_some());
    // the whole-PR batch is still held in its hook, before it is counted; the request may
    // also have started a neighbour batch, which can finish first where starting git is
    // fast (Linux, macOS)
    assert!(svc.batches_run() <= 1, "{}", svc.batches_run());
    release(&gate);
    wait_for("whole PR", || files.iter().all(|d| svc.is_cached(&d.path)));
}

#[test]
fn whole_pr_batch_is_dropped_on_close_and_reopen() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let (gate, entered) = gated_batches(&svc, "whole");
    let (_, files) = open_and_wait(&svc, &f);
    entered.recv_timeout(WAIT).expect("whole-PR batch started");
    svc.close();
    release(&gate);
    wait_for("batch end", || svc.batches_run() >= 1);
    assert_eq!(svc.batches_dropped(), 1);
    assert!(files.iter().all(|d| !svc.is_cached(&d.path)));
    assert!(svc.poll().is_empty());
    // a reopen while the old batch still runs: its result is dropped as well
    *gate.0.lock().unwrap() = false;
    let (_, files) = open_and_wait(&svc, &f);
    entered.recv_timeout(WAIT).expect("second batch started");
    svc.open(f.repo.clone(), f.pr.clone());
    release(&gate);
    collect_until(&svc, |e| opened(e).is_some());
    wait_for("old batch end", || svc.batches_dropped() >= 2);
    // the new session prefetches on its own
    wait_for("whole PR again", || {
        files.iter().all(|d| svc.is_cached(&d.path))
    });
}

#[test]
fn whole_pr_prefetch_keeps_to_the_cache_cap() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let cap = 8 * 1024;
    svc.set_cache_cap(cap);
    let (_, files) = open_and_wait(&svc, &f);
    wait_for("whole PR batch", || svc.batches_run() >= 1);
    assert!(svc.cache_bytes() <= cap, "{} > {cap}", svc.cache_bytes());
    let cached = files.iter().filter(|d| svc.is_cached(&d.path)).count();
    assert!(cached > 0 && cached < files.len(), "{cached}");
}

#[test]
fn panic_in_a_batch_keeps_workers_and_falls_back_per_file() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0);
    svc.set_batch_hook(Arc::new(|_| panic!("injected batch panic")));
    let (_, files) = open_and_wait(&svc, &f);
    let workers = svc.live_workers();
    svc.request_patch(files[20].path.clone(), true);
    let want = &files[20 - PREFETCH_RADIUS..=20 + PREFETCH_RADIUS];
    wait_for("per-file fallback", || {
        want.iter().all(|d| svc.is_cached(&d.path))
    });
    assert_eq!(svc.live_workers(), workers, "a panic killed a worker");
    assert!(svc.batches_run() >= 1);
    assert_eq!(svc.patches_built(), want.len() as u64);
}

#[test]
fn one_service_serves_many_opens_with_fresh_deps() {
    let f = review_fixture_with(2);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let head = f.head.clone();
    let deps: DepsFn = Arc::new(move |_: &RepoRef| {
        c.fetch_add(1, Ordering::SeqCst);
        let head = head.clone();
        Arc::new(ReviewDeps {
            auth: None,
            probe: Arc::new(move |_, _| Ok(head.clone())),
            comments: no_comments(),
            last_review: Arc::new(|_, _| Ok(None)),
        })
    });
    let svc = ReviewService::build(
        store(),
        egui::Context::default(),
        deps,
        Source::Local(f.repo_path.clone()),
    );
    let workers = {
        open_and_wait(&svc, &f);
        svc.live_workers()
    };
    for _ in 0..3 {
        svc.close();
        open_and_wait(&svc, &f);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4, "deps read once per open");
    assert_eq!(svc.live_workers(), workers, "the pool is reused");
}

#[test]
fn a_patch_is_served_while_every_batch_slot_is_busy() {
    if !two_workers() {
        return;
    }
    let f = review_fixture_with(120);
    let svc = local(&f);
    // every batch (the whole PR and each neighbour batch) waits at the gate
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, entered) = channel();
    let (g, tx) = (gate.clone(), Mutex::new(tx));
    svc.set_batch_hook(Arc::new(move |_| {
        let _ = tx.lock().unwrap().send(());
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    let (_, files) = open_and_wait(&svc, &f);
    let threads = svc.shared.threads;
    // more neighbour batches than workers: each served file queues one
    for k in 0..threads + 2 {
        let path = files[11 * k + 5].path.clone();
        svc.request_patch(path.clone(), true);
        let start = Instant::now();
        let mut evs = Vec::new();
        while patch_for(&evs, &path).is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "request {k} starved behind batches"
            );
            evs.extend(svc.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let running = entered.try_iter().count();
    assert_eq!(running, (threads - 1).max(1), "batches running at once");
    release(&gate);
}

#[test]
fn closing_stops_the_running_batch_before_its_git() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let git = Git::locate().unwrap();
    svc.use_git(git.clone());
    let (gate, entered) = gated_batches(&svc, "whole");
    open_and_wait(&svc, &f);
    entered.recv_timeout(WAIT).expect("whole-PR batch started");
    svc.close();
    let before = git.spawn_count();
    release(&gate);
    wait_for("batch end", || svc.batches_run() >= 1);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(git.spawn_count(), before, "a cancelled batch still ran git");
    assert_eq!(svc.batches_dropped(), 1);
}

#[test]
fn a_hung_recheck_of_one_pr_does_not_block_the_next_pr() {
    let f = review_fixture_with(2);
    let mut pr2 = f.pr.clone();
    pr2.id = 902;
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_probe = Arc::new((Mutex::new(false), Condvar::new()));
    let (h, r, head) = (hang.clone(), release_probe.clone(), f.head.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let svc = ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, id| {
            c.fetch_add(1, Ordering::SeqCst);
            if id == 901 && h.load(Ordering::SeqCst) {
                let (open, cv) = &*r;
                let mut o = open.lock().unwrap();
                while !*o {
                    o = cv.wait(o).unwrap();
                }
            }
            Ok(head.clone())
        }),
        no_comments(),
    );
    let mut pr1 = f.pr.clone();
    pr1.id = 901;
    svc.open(f.repo.clone(), pr1);
    collect_until(&svc, |e| opened(e).is_some());
    std::thread::sleep(Duration::from_millis(200));
    hang.store(true, Ordering::SeqCst);
    assert!(svc.recheck(), "first recheck of PR A");
    assert!(!svc.recheck(), "coalesced while A's probe is out");
    // PR B opens; A's probe still hangs
    svc.open(f.repo.clone(), pr2);
    collect_until(&svc, |e| opened(e).is_some());
    std::thread::sleep(Duration::from_millis(200));
    let before = calls.load(Ordering::SeqCst);
    assert!(svc.recheck(), "B's recheck is blocked by A's hung probe");
    wait_for("B's probe", || calls.load(Ordering::SeqCst) > before);
    *release_probe.0.lock().unwrap() = true;
    release_probe.1.notify_all();
}

#[test]
fn a_failed_git_lookup_is_retried_at_the_next_open() {
    let f = review_fixture_with(2);
    let svc = local(&f);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    svc.set_locator(Arc::new(move || {
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(GitError::Io("transient".into()))
        } else {
            Git::locate()
        }
    }));
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    assert!(failed(&evs), "{evs:?}");
    open_and_wait(&svc, &f);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // found once, then kept
    open_and_wait(&svc, &f);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

fn clone_of(root: &Path, name: &str, url: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::create_dir_all(&p).unwrap();
    let git = Git::locate().unwrap();
    git.run(&p, &["init", "-q"], None).unwrap();
    git.run(&p, &["remote", "add", "origin", url], None)
        .unwrap();
    p
}

#[test]
fn a_deleted_clone_is_dropped_from_the_scan_cache() {
    let root = tempfile::tempdir().unwrap();
    let url = "https://bitbucket.org/acme/api.git";
    let p = clone_of(root.path(), "api", url);
    let cache = Mutex::new(None);
    let git = Git::locate().unwrap();
    let found = clones_in(&cache, &git, root.path(), url, CLONE_RESCAN_EVERY);
    assert_eq!(found.len(), 1);
    std::fs::remove_dir_all(&p).unwrap();
    // the cached entry is gone at once; no rescan within the interval finds anything
    let found = clones_in(&cache, &git, root.path(), url, CLONE_RESCAN_EVERY);
    assert!(found.is_empty(), "{found:?}");
    assert!(cache.lock().unwrap().as_ref().unwrap().found.is_empty());
    // moved elsewhere under the root: found by the next rescan
    clone_of(root.path(), "moved/api", url);
    let found = clones_in(&cache, &git, root.path(), url, Duration::ZERO);
    assert_eq!(found.len(), 1);
    assert!(found[0].path.ends_with("moved/api") || found[0].path.ends_with("moved\\api"));
}

#[test]
fn a_clone_added_after_the_first_scan_is_found_by_one_rate_limited_rescan() {
    let root = tempfile::tempdir().unwrap();
    let url = "https://bitbucket.org/acme/api.git";
    let cache = Mutex::new(None);
    let git = Git::locate().unwrap();
    assert!(clones_in(&cache, &git, root.path(), url, CLONE_RESCAN_EVERY).is_empty());
    clone_of(root.path(), "api", url);
    // within the interval: no second walk
    let spawns = git.spawn_count();
    assert!(clones_in(&cache, &git, root.path(), url, CLONE_RESCAN_EVERY).is_empty());
    assert_eq!(git.spawn_count(), spawns, "rescanned within the interval");
    // after it: one rescan finds the clone; a matching clone needs no rescan
    assert_eq!(
        clones_in(&cache, &git, root.path(), url, Duration::ZERO).len(),
        1
    );
    let spawns = git.spawn_count();
    assert_eq!(
        clones_in(&cache, &git, root.path(), url, Duration::ZERO).len(),
        1
    );
    assert_eq!(git.spawn_count(), spawns);
    // the shared cache is invalidated by path
    forget_clone_in(&cache, &root.path().join("api"));
    assert!(cache.lock().unwrap().as_ref().unwrap().found.is_empty());
}

#[test]
fn a_fetch_io_error_on_a_clone_is_retried_once() {
    let gone = LocalRepo {
        path: PathBuf::from("Z:/definitely/not/here"),
        kind: RepoKind::Clone,
    };
    let here = tempfile::tempdir().unwrap();
    let clone = LocalRepo {
        path: here.path().to_path_buf(),
        kind: RepoKind::Clone,
    };
    let mirror = LocalRepo {
        path: here.path().to_path_buf(),
        kind: RepoKind::Mirror,
    };
    let io = GitError::Io("the directory name is invalid".into());
    assert!(retry_on_other_copy(&clone, &io));
    assert!(retry_on_other_copy(&gone, &GitError::NotInstalled));
    assert!(
        !retry_on_other_copy(&mirror, &io),
        "a mirror is not rescanned"
    );
    let failed = GitError::Failed {
        code: Some(128),
        stderr: "auth".into(),
    };
    assert!(!retry_on_other_copy(&clone, &failed), "a real fetch error");
}

#[test]
fn whole_pr_eligibility_follows_numstat() {
    let f = |a: u32, d: u32| DiffFile {
        path: "x".into(),
        old_path: None,
        status: FileStatus::Modified,
        additions: a,
        deletions: d,
        binary: false,
        counts_known: true,
        new_oid: String::new(),
    };
    assert!(whole_pr_eligible(&[f(10_000, 10_000)], WHOLE_PR_LINES));
    assert!(!whole_pr_eligible(&[f(10_000, 10_001)], WHOLE_PR_LINES));
    assert!(
        !whole_pr_eligible(&[f(20_001, 0)], u64::MAX),
        "a plain file"
    );
    assert!(!whole_pr_eligible(&[], WHOLE_PR_LINES));
    assert!(!whole_pr_eligible(&[f(1, 0)], 0));
    let unknown = DiffFile {
        counts_known: false,
        ..f(0, 0)
    };
    assert!(
        !whole_pr_eligible(&[unknown], WHOLE_PR_LINES),
        "decided only once the counts are in"
    );
}

fn counts_in(evs: &[ReviewEvent]) -> Option<Arc<Vec<FileCounts>>> {
    evs.iter().find_map(|e| match e {
        ReviewEvent::Counts { counts } => Some(counts.clone()),
        _ => None,
    })
}

/// Holds the numstat thread at its start until released; `entered` reports the start.
fn gated_counts(svc: &ReviewService) -> (Arc<(Mutex<bool>, Condvar)>, Receiver<()>) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = channel();
    let (g, tx) = (gate.clone(), Mutex::new(tx));
    svc.set_counts_hook(Arc::new(move |_| {
        let _ = tx.lock().unwrap().send(());
        let (open, cv) = &*g;
        let mut o = open.lock().unwrap();
        while !*o {
            o = cv.wait(o).unwrap();
        }
    }));
    (gate, rx)
}

#[test]
fn the_file_list_arrives_before_its_counts() {
    let f = review_fixture();
    let svc = local(&f);
    let (gate, entered) = gated_counts(&svc);
    svc.open(f.repo.clone(), f.pr.clone());
    // numstat is held: the list still arrives, with names, status and renames final
    let evs = collect_until(&svc, |e| opened(e).is_some() || failed(e));
    let (refs, files) = opened(&evs).unwrap_or_else(|| panic!("{evs:?}"));
    entered.recv_timeout(WAIT).expect("numstat started");
    assert_eq!(files.len(), crate::demo::DEMO_REVIEW_FILES);
    assert!(files
        .iter()
        .all(|d| !d.counts_known && d.additions == 0 && d.deletions == 0 && !d.binary));
    assert!(files
        .iter()
        .any(|d| d.status == FileStatus::Renamed && d.old_path.is_some()));
    std::thread::sleep(Duration::from_millis(200));
    assert!(counts_in(&svc.poll()).is_none(), "numstat is still held");
    release(&gate);
    let evs = collect_until(&svc, |e| counts_in(e).is_some() || failed(e));
    let counts = counts_in(&evs).unwrap_or_else(|| panic!("{evs:?}"));
    let mut got = files.clone();
    apply_counts(&mut got, &counts);
    let local = LocalRepo {
        path: f.repo_path.clone(),
        kind: RepoKind::Clone,
    };
    let mut want = gitlocal::file_index(&Git::locate().unwrap(), &local, &refs, None).unwrap();
    want.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(got, want, "the same files and counts as one joined listing");
    assert!(got.iter().any(|d| d.binary));
    // the service's own copy has the counts too
    let sess = svc.shared.lock().session.clone().unwrap();
    assert_eq!(sess.files, want);
}

#[test]
fn patches_are_served_while_numstat_runs() {
    let f = review_fixture();
    let svc = local(&f);
    let (gate, entered) = gated_counts(&svc);
    let (_, files) = open_listed(&svc, &f);
    entered.recv_timeout(WAIT).expect("numstat started");
    let m = files
        .iter()
        .find(|d| d.status == FileStatus::Modified && d.path.ends_with(".rs"))
        .unwrap();
    svc.request_patch(m.path.clone(), true);
    let evs = collect_until(&svc, |e| patch_for(e, &m.path).is_some());
    let (rows, plain) = patch_for(&evs, &m.path).unwrap();
    assert!(!plain && rows.iter().any(|r| r.kind == RowKind::Add));
    release(&gate);
    collect_until(&svc, |e| counts_in(e).is_some());
}

#[test]
fn the_whole_pr_prefetch_waits_for_the_counts() {
    if !two_workers() {
        return;
    }
    let f = review_fixture();
    let svc = local(&f);
    let (gate, entered) = gated_counts(&svc);
    let (_, files) = open_listed(&svc, &f);
    entered.recv_timeout(WAIT).expect("numstat started");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(svc.batches_run(), 0, "the size of the PR is not known yet");
    assert!(files.iter().all(|d| !svc.is_cached(&d.path)));
    release(&gate);
    wait_for("whole PR", || files.iter().all(|d| svc.is_cached(&d.path)));
}

#[test]
fn a_binary_file_has_no_rows_to_post_on_while_its_counts_are_unknown() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_whole_pr_lines(0);
    let (gate, entered) = gated_counts(&svc);
    let rows = svc.row_source();
    open_listed(&svc, &f);
    entered.recv_timeout(WAIT).expect("numstat started");
    let bin = "assets/logo.bin";
    let early = rows(bin).unwrap();
    assert!(
        early.rows.is_empty(),
        "git's patch of a binary file has no rows, so no inline comment can be placed"
    );
    release(&gate);
    collect_until(&svc, |e| counts_in(e).is_some());
    assert!(rows(bin).unwrap().binary, "flagged once numstat is in");
}

#[test]
fn a_failed_numstat_leaves_the_list_and_reports_it() {
    let f = review_fixture();
    let svc = local(&f);
    svc.set_counts_hook(Arc::new(|_| panic!("injected numstat panic")));
    let evs = open_events(&svc, &f);
    let (_, files) = opened(&evs).unwrap_or_else(|| panic!("{evs:?}"));
    assert!(evs
        .iter()
        .any(|x| matches!(x, ReviewEvent::CountsFailed(_))));
    assert!(counts_in(&evs).is_none());
    assert!(!failed(&evs), "the open itself succeeded: {evs:?}");
    svc.request_patch(files[4].path.clone(), true);
    collect_until(&svc, |e| patch_for(e, &files[4].path).is_some());
}

#[test]
fn plain_follows_the_changed_rows() {
    let row = |kind| Row {
        kind,
        old_line: None,
        new_line: None,
        text: String::new(),
    };
    let mut rows = vec![row(RowKind::HunkHeader)];
    rows.extend((0..PLAIN_CHANGED_LINES / 2).map(|_| row(RowKind::Add)));
    rows.extend((0..PLAIN_CHANGED_LINES / 2).map(|_| row(RowKind::Del)));
    rows.extend((0..100).map(|_| row(RowKind::Context)));
    assert!(!plain_rows(&rows), "exactly at the limit");
    rows.push(row(RowKind::Del));
    assert!(plain_rows(&rows));
}

#[test]
fn colours_follow_the_selection_when_an_earlier_request_is_served_later() {
    let f = review_fixture();
    let svc = local(&f);
    // no whole-PR prefetch: it could cache `a` and skip the held build below
    svc.set_whole_pr_lines(0);
    let (_, files) = open_and_wait(&svc, &f);
    let mut modified = files
        .iter()
        .filter(|d| d.status == FileStatus::Modified && !d.binary);
    let a = modified.next().unwrap().path.clone();
    let b = modified.nth(20).unwrap().path.clone();
    let slow = a.clone();
    // a's patch is held until the test has seen b's patch (no timing assumption)
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold = release.clone();
    svc.set_patch_hook(Arc::new(move |p| {
        if p == slow {
            let t = Instant::now();
            while !hold.load(std::sync::atomic::Ordering::SeqCst) && t.elapsed() < WAIT {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }));
    // b's colours are still being made when a's patch arrives
    let late = b.clone();
    svc.set_highlight_hook(Arc::new(move |p| {
        if p == late {
            std::thread::sleep(Duration::from_millis(800));
        }
    }));
    // selected a, then b (on screen now); a's patch arrives last
    svc.request_patch(a.clone(), true);
    svc.request_patch(b.clone(), true);
    let evs = collect_until(&svc, |e| {
        if patch_for(e, &b).is_some() {
            release.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        patch_for(e, &a).is_some() && highlighted_for(e, &b).is_some()
    });
    let at = |p: &str| {
        evs.iter()
            .position(|e| matches!(e, ReviewEvent::Patch { path, .. } if path == p))
            .unwrap()
    };
    assert!(at(&b) < at(&a), "{evs:?}");
}

fn bare_session(paths: &[&str]) -> Session {
    let repo = RepoRef::parse_bitbucket("acme/api").unwrap();
    let deps = fixed_deps(None, probe_returning("h".into()), no_comments())(&repo);
    Session {
        repo: RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: "w".into(),
            slug: "s".into(),
        },
        pr_id: 1,
        local: LocalRepo {
            path: PathBuf::from("."),
            kind: RepoKind::Clone,
        },
        refs: PrRefs {
            base: "b".into(),
            head: "h".into(),
            dest: "d".into(),
        },
        files: Vec::new(),
        index: paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.to_string(), i))
            .collect(),
        deps,
    }
}

fn task_name(t: Option<Task>) -> String {
    match t {
        Some(Task::Patch { path, .. }) => format!("patch {path}"),
        Some(Task::Highlight { job, .. }) => format!("colours {}", job.path),
        Some(Task::Batch { .. }) => "batch".into(),
        None => "nothing".into(),
    }
}

#[test]
fn workers_take_the_screen_first_and_speculative_work_last() {
    let state = || {
        let mut st = State::new();
        st.session = Some(Arc::new(bare_session(&["a", "b", "c", "d", "x"])));
        let rows = Arc::new(Vec::new());
        st.batches.push_back(Batch {
            paths: vec!["d".into()],
            whole_pr: true,
        });
        st.hl.request_background("c", &rows); // a prefetched neighbour's colours
        st.prefetch_one("b"); // a prefetch patch
        st.hl.request("a", &rows); // the colours of the file on screen
        st.enqueue("x".into(), true, true); // the patch the screen asked for
        st
    };
    let mut st = state();
    let order: Vec<String> = (0..6).map(|_| task_name(st.take_task(false, 1))).collect();
    assert_eq!(
        order,
        [
            "patch x",
            "colours a",
            "patch b",
            "colours c",
            "batch",
            "nothing"
        ],
        "the file on screen's colours come before any prefetch"
    );
    // the reserved worker builds patches only
    let mut st = state();
    let order: Vec<String> = (0..3).map(|_| task_name(st.take_task(true, 1))).collect();
    assert_eq!(order, ["patch x", "patch b", "nothing"]);
    // no batch beyond the free slots
    let mut st = state();
    st.batches_running = 1;
    let order: Vec<String> = (0..5).map(|_| task_name(st.take_task(false, 1))).collect();
    assert_eq!(order[4], "nothing");
}

/// A fixture whose PR adds `name` with `lines` lines of TypeScript.
fn with_ts_file(name: &str, lines: usize, modified_every: Option<usize>) -> (DemoReview, String) {
    let f = review_fixture_with(20);
    let line = |i: usize, tag: &str| {
        format!(
            "export const value{i} = compute<number>(`row ${{{i}}}`, (x) => x * {i}); // {tag}\n"
        )
    };
    if let Some(every) = modified_every {
        let base: String = (0..lines).map(|i| line(i, "base")).collect();
        std::fs::write(f.repo_path.join(name), base).unwrap();
        f.git(&["add", "-A"]);
        f.git(&["commit", "-qm", "base ts"]);
        f.git(&["checkout", "-q", "feature/demo"]);
        f.git(&["merge", "-q", "--no-edit", "main"]);
        let pr: String = (0..lines)
            .map(|i| line(i, if i % every == 0 { "changed" } else { "base" }))
            .collect();
        std::fs::write(f.repo_path.join(name), pr).unwrap();
    } else {
        f.git(&["checkout", "-q", "feature/demo"]);
        let text: String = (0..lines).map(|i| line(i, "new")).collect();
        std::fs::write(f.repo_path.join(name), text).unwrap();
    }
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "ts"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    (f, head)
}

fn local_at(f: &DemoReview, head: String) -> ReviewService {
    ReviewService::with_local(
        store(),
        f.repo_path.clone(),
        egui::Context::default(),
        probe_returning(head),
        no_comments(),
    )
}

fn ended(svc: &ReviewService, path: &str) -> Option<Instant> {
    let done = svc.shared.hl_done.lock().unwrap();
    done.iter().find(|(p, _)| p == path).map(|(_, t)| *t)
}

#[test]
fn a_running_highlight_stops_within_100_ms_of_a_close() {
    if !two_workers() {
        return;
    }
    let (f, head) = with_ts_file("big.ts", 19_000, None);
    let svc = local_at(&f, head);
    open_and_wait(&svc, &f);
    let (tx, started) = channel();
    let tx = Mutex::new(tx);
    svc.set_highlight_hook(Arc::new(move |p| {
        let _ = tx.lock().unwrap().send(p.to_string());
    }));
    svc.request_patch("big.ts".into(), true);
    let evs = collect_until(&svc, |e| patch_for(e, "big.ts").is_some());
    assert!(
        !patch_for(&evs, "big.ts").unwrap().1,
        "19,000 rows are not plain"
    );
    while started.recv_timeout(WAIT).expect("highlight started") != "big.ts" {}
    std::thread::sleep(Duration::from_millis(300));
    assert!(ended(&svc, "big.ts").is_none(), "still running");
    let closed = Instant::now();
    svc.close();
    wait_for("the highlight to stop", || ended(&svc, "big.ts").is_some());
    let latency = ended(&svc, "big.ts").unwrap() - closed;
    println!("highlight cancel latency: {latency:?}");
    assert!(latency < Duration::from_millis(100), "{latency:?}");
}

#[test]
fn a_highlight_is_abandoned_when_its_file_leaves_the_prefetch_window() {
    if !two_workers() {
        return;
    }
    let (f, head) = with_ts_file("big.ts", 19_000, None);
    let svc = local_at(&f, head);
    let (_, files) = open_and_wait(&svc, &f);
    let at = files.iter().position(|d| d.path == "big.ts").unwrap();
    svc.request_patch("big.ts".into(), true);
    collect_until(&svc, |e| any_highlight_for(e, "big.ts"));
    // far beyond the window of five files on each side
    let far = files[at + PREFETCH_RADIUS + 3].path.clone();
    svc.request_patch(far.clone(), true);
    collect_until(&svc, |e| patch_for(e, &far).is_some());
    wait_for("the abandoned highlight", || {
        ended(&svc, "big.ts").is_some()
    });
    let mut st = svc.shared.lock();
    let rows = st.cache.get("big.ts").unwrap().0;
    assert!(
        st.hl.request("big.ts", &rows).is_none(),
        "not remembered: it was not finished"
    );
}

#[test]
fn the_hunk_on_screen_is_coloured_first() {
    let (f, head) = with_ts_file("many.ts", 4_000, Some(40));
    let svc = local_at(&f, head);
    // the colours of the whole file must finish however slow the machine is
    svc.shared.hl_budget_ms.store(120_000, Ordering::Relaxed);
    open_and_wait(&svc, &f);
    // the screen shows row 500 of the file before its colours start
    svc.set_visible("many.ts", 500);
    svc.request_patch("many.ts".into(), true);
    let evs = collect_until(&svc, |e| highlighted_for(e, "many.ts").is_some());
    let (rows, _) = patch_for(&evs, "many.ts").unwrap();
    let hunk_start = (0..=500)
        .rev()
        .find(|&i| rows[i].kind == RowKind::HunkHeader)
        .unwrap();
    let first = evs
        .iter()
        .find_map(|e| match e {
            ReviewEvent::Highlighted { path, range, .. } if path == "many.ts" => {
                Some(range.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(first.start, hunk_start, "{first:?}");
    assert!(first.len() <= CHUNK_ROWS);
    // and the merged steps are exactly the whole-file colours
    let whole = super::super::highlight::highlight_rows("many.ts", &rows);
    assert_eq!(*highlighted_for(&evs, "many.ts").unwrap(), whole);
}

#[test]
fn a_file_over_its_time_budget_keeps_what_was_coloured() {
    let (f, head) = with_ts_file("big.ts", 19_000, None);
    let svc = local_at(&f, head);
    svc.shared.hl_budget_ms.store(50, Ordering::Relaxed);
    open_and_wait(&svc, &f);
    svc.request_patch("big.ts".into(), true);
    collect_until(&svc, |e| any_highlight_for(e, "big.ts"));
    wait_for("the job to stop", || ended(&svc, "big.ts").is_some());
    // remembered as it is: going back costs no new 3 s
    let mut st = svc.shared.lock();
    let rows = st.cache.get("big.ts").unwrap().0;
    let kept = st.hl.request("big.ts", &rows).expect("remembered");
    let coloured = kept.iter().filter(|s| !s.is_empty()).count();
    assert!(coloured > 0 && coloured < rows.len(), "{coloured}");
}

#[test]
fn open_lists_the_commits_and_reads_my_last_review() {
    let f = review_fixture();
    let head = f.head.clone();
    let mark = prcore::history::ReviewMark {
        at: chrono::Utc::now(),
        commit: Some(f.head.clone()),
    };
    let m = mark.clone();
    let deps: DepsFn = Arc::new(move |_: &RepoRef| {
        let head = head.clone();
        let m = m.clone();
        Arc::new(ReviewDeps {
            auth: None,
            probe: Arc::new(move |_, _| Ok(head.clone())),
            comments: no_comments(),
            last_review: Arc::new(move |_, _| Ok(Some(m.clone()))),
        })
    });
    let svc = ReviewService::build(
        store(),
        egui::Context::default(),
        deps,
        Source::Local(f.repo_path.clone()),
    );
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, |e| {
        e.iter().any(|x| matches!(x, ReviewEvent::LastReview(_))) || failed(e)
    });
    let commits = evs
        .iter()
        .find_map(|e| match e {
            ReviewEvent::Commits(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no commits in {evs:?}"));
    assert!(!commits.is_empty());
    assert_eq!(
        commits.last().unwrap().id,
        f.head,
        "oldest first, head last"
    );
    assert!(evs
        .iter()
        .any(|e| matches!(e, ReviewEvent::LastReview(Some(got)) if *got == mark)));
}

#[test]
fn a_failing_last_review_read_does_not_fail_the_open() {
    let f = review_fixture();
    let head = f.head.clone();
    let deps: DepsFn = Arc::new(move |_: &RepoRef| {
        let head = head.clone();
        Arc::new(ReviewDeps {
            auth: None,
            probe: Arc::new(move |_, _| Ok(head.clone())),
            comments: no_comments(),
            last_review: Arc::new(|_, _| Err("network error".into())),
        })
    });
    let svc = ReviewService::build(
        store(),
        egui::Context::default(),
        deps,
        Source::Local(f.repo_path.clone()),
    );
    svc.open(f.repo.clone(), f.pr.clone());
    let evs = collect_until(&svc, |e| {
        e.iter()
            .any(|x| matches!(x, ReviewEvent::LastReviewFailed(_)))
            || failed(e)
    });
    assert!(!failed(&evs));
    assert!(evs.iter().any(|e| matches!(e, ReviewEvent::Commits(_))));
}
