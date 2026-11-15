use super::*;
use store::MemorySecrets;

#[test]
fn me_round_trips_through_meta() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(load_me(&store), None);
    let me = UserRef {
        id: "{abc-123}".into(),
        display_name: "Ana Souza".into(),
        login: None,
    };
    let m = Mutex::new(store);
    save_me(&m, &me);
    assert_eq!(load_me(&m.lock().unwrap()), Some(me));
}

pub(crate) fn backend_with_saved_login() -> (Backend, Arc<MemorySecrets>) {
    let secrets = Arc::new(MemorySecrets::new());
    let (ek, tk) = bitbucket_keys();
    secrets.set(&ek, "ana@example.com").unwrap();
    secrets.set(&tk, "not-a-real-token").unwrap();
    let b = Backend::new(
        eframe::egui::Context::default(),
        secrets.clone(),
        Store::open_in_memory().unwrap(),
    )
    .unwrap();
    let me = UserRef {
        id: "{abc}".into(),
        display_name: "Ana".into(),
        login: None,
    };
    save_me(&b.store, &me);
    (b, secrets)
}

#[test]
fn a_pre_account_sign_in_survives_the_upgrade() {
    let secrets = Arc::new(MemorySecrets::new());
    secrets.set(LEGACY_BB_EMAIL, "ana@example.com").unwrap();
    secrets.set(LEGACY_BB_TOKEN, "not-a-real-token").unwrap();
    let store = Store::open_in_memory().unwrap();
    // as the app's start does, before the backend
    let moved = crate::accounts::migrate_legacy_bitbucket(secrets.as_ref(), &store);
    assert_eq!(moved, crate::accounts::MigrateOutcome::Moved);
    let mut b = Backend::new(eframe::egui::Context::default(), secrets.clone(), store).unwrap();
    assert!(b.has_stored_credentials(), "still signed in");
    assert_eq!(b.stored_email().as_deref(), Some("ana@example.com"));
    assert_eq!(secrets.get(LEGACY_BB_TOKEN).unwrap(), None);
    // a legacy entry left by a failed move is removed by signing out too
    secrets.set(LEGACY_BB_TOKEN, "left-over").unwrap();
    b.sign_out().unwrap();
    assert_eq!(secrets.get(LEGACY_BB_TOKEN).unwrap(), None);
    assert!(!b.has_stored_credentials());
}

#[test]
fn sign_out_deletes_email_token_and_me() {
    let (mut b, secrets) = backend_with_saved_login();
    assert!(b.has_stored_credentials() && b.stored_me().is_some());
    b.sign_out().unwrap();
    let (ek, tk) = bitbucket_keys();
    assert_eq!(secrets.get(&ek).unwrap(), None);
    assert_eq!(secrets.get(&tk).unwrap(), None);
    assert!(!b.has_stored_credentials());
    assert_eq!(b.stored_me(), None);
}

#[test]
fn sign_out_drops_messages_from_the_old_session() {
    let (mut b, _) = backend_with_saved_login();
    let _ = b.tx.send(Msg::Connected(b.stored_me().unwrap()));
    b.sign_out().unwrap();
    assert!(b.rx.try_recv().is_err());
}

/// Counts reads; the UI thread must never trigger one through `comment_poster`.
struct CountingSecrets {
    inner: MemorySecrets,
    reads: std::sync::atomic::AtomicUsize,
}

impl SecretStore for CountingSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, store::SecretError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), store::SecretError> {
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), store::SecretError> {
        self.inner.delete(key)
    }
}

#[test]
fn comment_poster_reads_no_credentials_on_the_calling_thread() {
    let secrets = Arc::new(CountingSecrets {
        inner: MemorySecrets::new(),
        reads: Default::default(),
    });
    let b = Backend::new(
        eframe::egui::Context::default(),
        secrets.clone(),
        Store::open_in_memory().unwrap(),
    )
    .unwrap();
    let poster = b.comment_poster().expect("a poster");
    assert_eq!(
        secrets.reads.load(Ordering::SeqCst),
        0,
        "no read on the UI thread"
    );
    // the first call (on a worker) reads them; without credentials it sends nothing
    let repo = RepoRef::parse_bitbucket("acme/api").unwrap();
    let err = std::thread::spawn(move || poster.set_review(&repo, 1, ReviewAction::Approve, None))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(secrets.reads.load(Ordering::SeqCst) > 0);
    assert!(matches!(err, PostError::Network(_)), "{err:?}");
}

#[test]
fn post_errors_map_to_categories_without_detail() {
    let secret = "https://x?token=secret";
    let cases = [
        (ProviderError::PostedUnreadable, PostError::PostedUnreadable),
        (
            ProviderError::Network(secret.into()),
            PostError::Network("network error".into()),
        ),
        (
            ProviderError::Invalid(secret.into()),
            PostError::Invalid("invalid request".into()),
        ),
        (
            ProviderError::Forbidden,
            PostError::Rejected("access denied".into()),
        ),
        (
            ProviderError::Decode(secret.into()),
            PostError::Rejected("decode error".into()),
        ),
        (ProviderError::Unconfirmed, PostError::Unconfirmed),
        (
            ProviderError::Rejected {
                status: 400,
                message: "You can't approve your own pull request.".into(),
            },
            PostError::Rejected("http 400: You can't approve your own pull request.".into()),
        ),
        (
            ProviderError::Rejected {
                status: 403,
                message: "Insufficient scope".into(),
            },
            PostError::Rejected("access denied: Insufficient scope".into()),
        ),
    ];
    for (e, want) in cases {
        assert_eq!(post_error(&e), want);
    }
    let (b, _) = backend_with_saved_login();
    assert!(b.comment_poster().is_some());
}

/// The inbox read as it was before the joined query: `list_prs` plus one `get_diffstat`
/// per PR (the reference the joined query must match).
fn n_plus_one_snapshot(store: &Store) -> Snapshot {
    let prs = store.list_prs().unwrap();
    let mut stats = HashMap::new();
    for p in &prs {
        if let Ok(Some((_, d))) = store.get_diffstat(&p.repo, p.id) {
            stats.insert((p.repo.clone(), p.id), d);
        }
    }
    Snapshot {
        prs,
        stats,
        errors: store.repo_errors().unwrap(),
        repo_count: store.selected_repos().unwrap().len(),
        known_repos: store.repo_choices().unwrap().len(),
        preselected: None,
        account: store.get_meta("account_id").unwrap(),
        account_users: HashMap::new(),
    }
}

#[test]
fn a_snapshot_build_running_at_sign_out_answers_into_the_new_channel() {
    let (old_tx, mut old_rx) = unbounded_channel();
    let slot = Arc::new(Mutex::new(old_tx));
    let (busy, again) = (
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
    );
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let (s, b, a) = (slot.clone(), busy.clone(), again.clone());
    let builder = std::thread::spawn(move || {
        let read = || {
            let _ = entered_tx.send(());
            let _ = go_rx.lock().unwrap().recv();
            Ok(Snapshot::default())
        };
        snapshot_builds(read, &s, &eframe::egui::Context::default(), &b, &a);
    });
    entered_rx.recv().unwrap();
    // sign-out while the first build runs, then a request of the new session
    let (new_tx, mut new_rx) = unbounded_channel();
    *slot.lock().unwrap() = new_tx;
    again.store(true, Ordering::SeqCst);
    go_tx.send(()).unwrap();
    entered_rx.recv().unwrap();
    go_tx.send(()).unwrap();
    builder.join().unwrap();
    assert!(matches!(new_rx.try_recv(), Ok(Msg::Snapshot(Ok(_)))));
    assert!(matches!(new_rx.try_recv(), Ok(Msg::Snapshot(Ok(_)))));
    assert!(
        old_rx.try_recv().is_err(),
        "sent into the old session's channel"
    );
    assert!(!busy.load(Ordering::SeqCst));
}

/// A provider answering from memory: one open PR per repository, or `Auth` everywhere;
/// records the calls it gets.
struct FakeProvider {
    auth_fails: bool,
    calls: Mutex<Vec<String>>,
    /// What `paused_until` reports (a rate-limited client).
    paused: Option<std::time::SystemTime>,
}

impl FakeProvider {
    fn new(auth_fails: bool) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            auth_fails,
            calls: Mutex::new(Vec::new()),
            paused: None,
        })
    }
    fn note(&self, c: String) -> Result<(), ProviderError> {
        lock_ok(&self.calls).push(c);
        if self.auth_fails {
            Err(ProviderError::Auth)
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn forge(&self) -> Forge {
        Forge::BitbucketCloud
    }
    fn capabilities(&self) -> providers::provider::Capabilities {
        providers::provider::Capabilities::BITBUCKET
    }
    fn paused_until(&self) -> Option<std::time::SystemTime> {
        self.paused
    }
    async fn me(&self) -> Result<UserRef, ProviderError> {
        self.note("me".into())?;
        Ok(UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        })
    }
    async fn discover(&self) -> Result<providers::provider::Discovered, ProviderError> {
        self.note("discover".into())?;
        Ok(Default::default())
    }
    async fn prs(
        &self,
        r: &RepoRef,
        _: prcore::model::PrState,
        _: usize,
    ) -> Result<providers::provider::PrList, ProviderError> {
        self.note(format!("prs:{}", r.full_name()))?;
        let mut pr = crate::demo::demo_data().1[0].clone();
        pr.repo = r.clone();
        pr.source_repo = r.clone();
        pr.id = 1;
        Ok(providers::provider::PrList {
            prs: vec![pr],
            skipped: vec![],
        })
    }
    async fn merged_by_source_branch(
        &self,
        _: &RepoRef,
        _: &str,
    ) -> Result<providers::provider::PrList, ProviderError> {
        Ok(Default::default())
    }
    async fn diffstat(&self, _: &RepoRef, _: u64) -> Result<DiffStat, ProviderError> {
        Ok(DiffStat {
            files: 1,
            lines_added: 1,
            lines_removed: 0,
            max_file_lines: 1,
        })
    }
    async fn pr(&self, r: &RepoRef, id: u64) -> Result<PullRequest, ProviderError> {
        self.note(format!("pr:{}#{id}", r.full_name()))?;
        Err(ProviderError::NotFound)
    }
    async fn revision(&self, _: &RepoRef, _: u64) -> Result<ServerRevision, ProviderError> {
        Err(ProviderError::NotFound)
    }
    async fn comments(
        &self,
        _: &RepoRef,
        _: u64,
    ) -> Result<providers::provider::CommentList, ProviderError> {
        Ok(Default::default())
    }
    async fn tasks(
        &self,
        _: &RepoRef,
        _: u64,
    ) -> Result<Vec<prcore::comments::PrTask>, ProviderError> {
        Ok(vec![])
    }
    async fn post_comment(
        &self,
        r: &RepoRef,
        id: u64,
        _: &PostRevision,
        _: &NewComment,
    ) -> Result<Comment, ProviderError> {
        self.note(format!("post:{}#{id}", r.full_name()))?;
        Err(ProviderError::Unconfirmed)
    }
    async fn set_resolved(
        &self,
        _: &RepoRef,
        _: u64,
        _: &ThreadId,
        _: bool,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn set_review(
        &self,
        _: &RepoRef,
        _: u64,
        _: ReviewAction,
        _: Option<&str>,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
}
mod accounts;

#[test]
fn signing_in_as_another_account_clears_the_cache_first() {
    let store = Store::open_in_memory().unwrap();
    populate(&store, 2, 2);
    let m = Mutex::new(store);
    let ana = UserRef {
        id: "{ana}".into(),
        display_name: "Ana".into(),
        login: None,
    };
    adopt_account(&m, &AccountId::default_bitbucket(), &ana).unwrap();
    populate(&m.lock().unwrap(), 2, 2);
    adopt_account(&m, &AccountId::default_bitbucket(), &ana).unwrap();
    let reader = m.lock().unwrap().reader();
    let snap = read_snapshot(&reader).unwrap();
    assert_eq!(snap.prs.len(), 4, "the same account keeps its cache");
    assert_eq!(snap.account.as_deref(), Some("{ana}"));
    let bo = UserRef {
        id: "{bo}".into(),
        display_name: "Bo".into(),
        login: None,
    };
    adopt_account(&m, &AccountId::default_bitbucket(), &bo).unwrap();
    let snap = read_snapshot(&reader).unwrap();
    assert!(snap.prs.is_empty() && snap.errors.is_empty() && snap.repo_count == 0);
    assert_eq!(snap.account.as_deref(), Some("{bo}"));
    assert_eq!(load_me(&m.lock().unwrap()), Some(bo));
}

/// `repos` x `per_repo` PRs, every one with a diffstat, a few repo errors.
pub(crate) fn populate(store: &Store, repos: usize, per_repo: u64) {
    let (_, demo, _) = crate::demo::demo_data();
    let template = demo[0].clone();
    let refs: Vec<RepoRef> = (0..repos)
        .map(|i| RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: "acme".into(),
            slug: format!("repo{i:03}"),
        })
        .collect();
    store.upsert_repos(&refs).unwrap();
    store.set_selection(&refs).unwrap();
    for (i, r) in refs.iter().enumerate() {
        let prs: Vec<PullRequest> = (1..=per_repo)
            .map(|id| {
                let mut p = template.clone();
                p.repo = r.clone();
                p.id = id;
                p.title = format!("PR {id} of {}", r.slug);
                p.source_commit = format!("c{id}");
                p
            })
            .collect();
        let stats: Vec<store::DiffstatRow> = prs
            .iter()
            .map(|p| {
                let d = DiffStat {
                    files: p.id as u32,
                    lines_added: 10,
                    lines_removed: 2,
                    max_file_lines: 7,
                };
                (p.id, p.source_commit.clone(), d)
            })
            .collect();
        let error = (i % 10 == 0).then_some("rate limited");
        store.apply_repo_sync(r, Some(&prs), &stats, error).unwrap();
    }
}

/// The next message of `b` that `pick` accepts (others are skipped), within 10 s.
fn wait_for<T>(b: &mut Backend, pick: impl Fn(Msg) -> Option<T>) -> T {
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_secs(10) {
        match b.rx.try_recv() {
            Ok(m) => {
                if let Some(v) = pick(m) {
                    return v;
                }
            }
            Err(_) => std::thread::yield_now(),
        }
    }
    panic!("no such message");
}

#[test]
fn a_saved_selection_hides_unselected_repos_and_purges_on_request() {
    let (mut b, _) = backend_with_saved_login();
    populate(&b.store.lock().unwrap(), 3, 2);
    let keep = RepoRef::parse_bitbucket("acme/repo000").unwrap();
    let hide = RepoRef::parse_bitbucket("acme/repo001").unwrap();
    let purge = RepoRef::parse_bitbucket("acme/repo002").unwrap();
    b.save_selection(
        vec![(AccountId::default_bitbucket(), keep.clone())],
        vec![purge.clone()],
    );
    let saved = wait_for(&mut b, |m| match m {
        Msg::SelectionSaved(r) => Some(r),
        _ => None,
    });
    assert_eq!(saved, Ok(()));
    let snap = read_snapshot(&b.reader).unwrap();
    assert!(snap.prs.iter().all(|p| p.repo == keep));
    assert_eq!((snap.prs.len(), snap.repo_count), (2, 1));
    let cached = b.store.lock().unwrap().list_prs().unwrap();
    assert!(cached.iter().any(|p| p.repo == hide), "kept, hidden");
    assert!(!cached.iter().any(|p| p.repo == purge), "purged");
    b.request_repo_choices();
    let choices = wait_for(&mut b, |m| match m {
        Msg::RepoChoices(c) => Some(c.unwrap()),
        _ => None,
    });
    let selected: Vec<&RepoRef> = choices
        .iter()
        .filter(|c| c.selected)
        .map(|c| &c.repo)
        .collect();
    assert_eq!(selected, vec![&keep]);
}

#[test]
fn discovery_errors_name_the_category_only() {
    assert_eq!(
        discovery_error(&DiscoveryError::Workspaces("network error".into())),
        "Could not list your repositories (network error)"
    );
}

#[test]
fn joined_snapshot_equals_n_plus_one_result() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
    populate(&store, 12, 4);
    // one PR without a diffstat
    let lone = RepoRef::parse_bitbucket("acme/lone").unwrap();
    let mut p = crate::demo::demo_data().1[0].clone();
    p.repo = lone.clone();
    store.replace_prs(&lone, &[p]).unwrap();
    let mut all = store.selected_repos().unwrap();
    all.push(lone);
    store.set_selection(&all).unwrap();
    let old = n_plus_one_snapshot(&store);
    let new = read_snapshot(&store.reader()).unwrap();
    assert_eq!(new, old);
    assert_eq!(new.prs.len(), 49);
    assert_eq!(new.stats.len(), 48);
    // the inbox built from both is the same
    let me = old.prs[0].author.clone();
    for view in [crate::inbox::View::All, crate::inbox::View::NeedsMe] {
        let a = crate::inbox::build_inbox(&old.prs, &old.stats, &me, view, "");
        let b = crate::inbox::build_inbox(&new.prs, &new.stats, &me, view, "");
        assert_eq!(a, b);
    }
}

#[test]
fn requested_snapshots_arrive_as_messages() {
    let (mut b, _) = backend_with_saved_login();
    populate(&b.store.lock().unwrap(), 3, 2);
    for _ in 0..5 {
        b.request_snapshot();
    }
    let t = std::time::Instant::now();
    let mut last = None;
    while t.elapsed() < std::time::Duration::from_secs(10) {
        match b.rx.try_recv() {
            Ok(Msg::Snapshot(s)) => last = Some(s),
            Ok(_) => {}
            Err(_) if last.is_some() && !b.snap_busy.load(Ordering::SeqCst) => break,
            Err(_) => std::thread::yield_now(),
        }
    }
    let snap = last.expect("a snapshot arrived").unwrap();
    assert_eq!(snap.prs.len(), 6);
    assert_eq!(snap.repo_count, 3);
}

/// The audit's snapshot bench (section 1.5): 500 PRs in 50 repos in a file store, the
/// snapshot alone and while two threads write diffstats through the same mutex. Run with
/// `cargo test -p app --release -- --ignored --nocapture bench_snapshot`.
#[test]
#[ignore]
fn bench_snapshot_with_writers() {
    let base = std::env::var_os("NIT_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(base).unwrap();
    let opened = Store::open(&dir.path().join("cache.sqlite")).unwrap();
    let reader = opened.reader();
    let store = Arc::new(Mutex::new(opened));
    let t = std::time::Instant::now();
    populate(&store.lock().unwrap(), 50, 10);
    eprintln!(
        "populate 50 repos x 10 PRs (one transaction per repo): {:.1} ms in {}",
        t.elapsed().as_secs_f64() * 1000.0,
        dir.path().display()
    );
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], *v.last().unwrap())
    };
    let mut joined = Vec::new();
    let mut old = Vec::new();
    for _ in 0..20 {
        let t = std::time::Instant::now();
        assert_eq!(read_snapshot(&reader).unwrap().prs.len(), 500);
        joined.push(ms(t.elapsed()));
        let t = std::time::Instant::now();
        n_plus_one_snapshot(&store.lock().unwrap());
        old.push(ms(t.elapsed()));
    }
    eprintln!(
        "snapshot, joined query: median/max {:.2?} ms",
        median(joined)
    );
    eprintln!(
        "snapshot, old N+1 reads (WAL): median/max {:.2?} ms",
        median(old)
    );
    let mut per_write = Vec::new();
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                let r = RepoRef::parse_bitbucket(&format!("acme/repo{w:03}")).unwrap();
                let mut times = Vec::new();
                let mut i = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    i += 1;
                    let d = DiffStat {
                        files: i,
                        lines_added: i,
                        lines_removed: 0,
                        max_file_lines: i,
                    };
                    let t = std::time::Instant::now();
                    store
                        .lock()
                        .unwrap()
                        .put_diffstat(&r, (i % 10 + 1) as u64, &format!("w{i}"), &d)
                        .unwrap();
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                times
            })
        })
        .collect();
    let mut waits = Vec::new();
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let t = std::time::Instant::now();
        read_snapshot(&reader).unwrap();
        waits.push(ms(t.elapsed()));
    }
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        per_write.extend(w.join().unwrap());
    }
    eprintln!(
        "snapshot build (off the UI thread) with 2 writers: median/max {:.2?} ms",
        median(waits)
    );
    eprintln!(
        "diffstat write through the mutex: {} writes, median/max {:.3?} ms",
        per_write.len(),
        median(per_write)
    );
    // what the UI thread does now: ask, never read
    let secrets = Arc::new(store::MemorySecrets::new());
    let b = Backend::new(
        eframe::egui::Context::default(),
        secrets,
        Store::open(&dir.path().join("cache.sqlite")).unwrap(),
    )
    .unwrap();
    let guard = b.store.lock().unwrap();
    let t = std::time::Instant::now();
    b.request_snapshot();
    eprintln!(
        "UI thread cost of a refresh (store mutex held elsewhere): {:.3} ms",
        ms(t.elapsed())
    );
    drop(guard);
}
