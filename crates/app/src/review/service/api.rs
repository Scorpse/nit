use super::*;

pub struct ReviewService {
    pub(super) shared: Arc<Shared>,
    pub(super) rx: Mutex<Receiver<(u64, ReviewEvent)>>,
    /// The generation whose `recheck` is out: further ones for it are dropped until it is
    /// back (one in flight per open PR). A probe still out for an older PR blocks nothing.
    pub(super) rechecking: Arc<Mutex<Option<u64>>>,
}

/// Clears the recheck in-flight mark of its generation when the recheck ends (also on a
/// panic); a newer generation's mark is left alone.
pub(super) struct RecheckDone(Arc<Mutex<Option<u64>>>, u64);

impl Drop for RecheckDone {
    fn drop(&mut self) {
        let mut out = lock_ok(&self.0);
        if *out == Some(self.1) {
            *out = None;
        }
    }
}

impl ReviewService {
    /// Real path: HTTPS remote `https://bitbucket.org/<ws>/<slug>.git`, auth from `creds`
    /// (tests; the app reads the sign-in at each open, see `with_deps`).
    #[cfg(test)]
    pub fn new(
        store: Arc<Mutex<Store>>,
        creds: Option<GitAuth>,
        ctx: egui::Context,
        head_probe: HeadProbe,
        comments: CommentFetcher,
    ) -> ReviewService {
        Self::build(
            store,
            ctx,
            fixed_deps(creds, head_probe, comments),
            Source::Remote(Arc::new(https_url)),
        )
    }

    /// Real path with the sign-in read at each open (on the open's thread), so one
    /// service serves every review of a backend, across sign-outs and sign-ins.
    pub fn with_deps(store: Arc<Mutex<Store>>, ctx: egui::Context, deps: DepsFn) -> ReviewService {
        Self::build(store, ctx, deps, Source::Remote(Arc::new(https_url)))
    }

    /// Test path: uses `repo_path` (holding both branches) directly, no fetch.
    #[cfg(test)]
    pub fn with_local(
        store: Arc<Mutex<Store>>,
        repo_path: PathBuf,
        ctx: egui::Context,
        head_probe: HeadProbe,
        comments: CommentFetcher,
    ) -> ReviewService {
        Self::build(
            store,
            ctx,
            fixed_deps(None, head_probe, comments),
            Source::Local(repo_path),
        )
    }

    /// Demo path: like the test path, with my last review answered by `last_review`.
    pub fn with_local_reviewed(
        store: Arc<Mutex<Store>>,
        repo_path: PathBuf,
        ctx: egui::Context,
        head_probe: HeadProbe,
        comments: CommentFetcher,
        last_review: LastReviewFetcher,
    ) -> ReviewService {
        let deps = Arc::new(ReviewDeps {
            auth: None,
            probe: head_probe,
            comments,
            last_review,
            create_task: Arc::new(|_, _, _| Err("tasks are not available here".into())),
        });
        Self::build(
            store,
            ctx,
            Arc::new(move |_: &RepoRef| deps.clone()),
            Source::Local(repo_path),
        )
    }

    /// Like `new`, but with the remote URL chosen by `url_for` (tests).
    #[cfg(test)]
    pub(crate) fn with_url_for(
        store: Arc<Mutex<Store>>,
        creds: Option<GitAuth>,
        ctx: egui::Context,
        head_probe: HeadProbe,
        comments: CommentFetcher,
        url_for: UrlFor,
    ) -> ReviewService {
        Self::build(
            store,
            ctx,
            fixed_deps(creds, head_probe, comments),
            Source::Remote(url_for),
        )
    }

    pub(super) fn build(
        store: Arc<Mutex<Store>>,
        ctx: egui::Context,
        deps: DepsFn,
        source: Source,
    ) -> ReviewService {
        let (tx, rx) = channel();
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(4);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            work: Condvar::new(),
            tx,
            ctx,
            store,
            deps,
            comments_seq: AtomicU64::new(0),
            source,
            git: OnceLock::new(),
            whole_pr_lines: AtomicU64::new(WHOLE_PR_LINES),
            threads,
            built: AtomicU64::new(0),
            batches_run: AtomicU64::new(0),
            batches_dropped: AtomicU64::new(0),
            loads_active: AtomicUsize::new(0),
            loads_max: AtomicUsize::new(0),
            load_lock: Mutex::new(()),
            #[cfg(test)]
            load_waiters: AtomicUsize::new(0),
            #[cfg(test)]
            load_hook: Mutex::new(None),
            workers_live: AtomicUsize::new(0),
            visible: Mutex::new(None),
            hl_budget_ms: AtomicU64::new(HIGHLIGHT_BUDGET.as_millis() as u64),
            hl_ended: AtomicU64::new(0),
            #[cfg(test)]
            patch_hook: Mutex::new(None),
            #[cfg(test)]
            hl_hook: Mutex::new(None),
            #[cfg(test)]
            batch_hook: Mutex::new(None),
            #[cfg(test)]
            counts_hook: Mutex::new(None),
            #[cfg(test)]
            hl_done: Mutex::new(Vec::new()),
            #[cfg(test)]
            locator: Mutex::new(None),
        });
        for i in 0..threads {
            let s = shared.clone();
            // a pool that failed to start some threads still works with the rest
            // A highlight stops only between rows and a batch can run long, so with two or
            // more workers the first never takes either and patches are never stuck behind
            // them; highlights of different files run on the other workers at once. A single
            // worker does everything (a file's highlight is bounded: plain files skip it, and
            // `HIGHLIGHT_BUDGET` caps the rest).
            let reserved = threads >= 2 && i == 0;
            let _ = std::thread::Builder::new()
                .name(format!("review-worker-{i}"))
                .spawn(move || worker(&s, reserved));
        }
        // first colours should not pay for loading the syntaxes
        let _ = std::thread::Builder::new()
            .name("review-warm-highlight".into())
            .spawn(super::super::highlight::warm);
        ReviewService {
            shared,
            rx: Mutex::new(rx),
            rechecking: Arc::new(Mutex::new(None)),
        }
    }

    /// Opens `pr` in the background, cancelling whatever was open before.
    pub fn open(&self, repo: RepoRef, pr: PullRequest) {
        let gen = {
            let mut st = self.shared.lock();
            st.reset();
            st.gen
        };
        let s = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("review-open".into())
            .spawn(move || open_job(&s, gen, repo, pr));
        if spawned.is_err() {
            self.shared.emit(
                gen,
                ReviewEvent::Failed("Could not start a background thread".into()),
            );
        }
    }

    /// Asks for one file's rows; a cached patch is emitted at once. `priority` jumps the
    /// queue. Either way, its neighbours are prefetched once it has been served.
    pub fn request_patch(&self, path: String, priority: bool) {
        let mut st = self.shared.lock();
        let gen = st.gen;
        if priority {
            // the screen's selection: the file whose colours come first from now on
            st.focus = Some(path.clone());
        }
        if let Some(sess) = st.session.clone() {
            if let Some((rows, layout)) = st.cache.get(&path) {
                let plain = layout.plain;
                serve(
                    &self.shared,
                    &mut st,
                    gen,
                    &sess,
                    &path,
                    (rows, plain, layout),
                );
                drop(st);
                self.shared.work.notify_all();
                self.shared.ctx.request_repaint();
                return;
            }
        }
        st.enqueue(path, true, priority);
        self.shared.work.notify_all();
    }

    /// Lists the files changed in `range` (answered by `RangeFiles`).
    pub fn request_range_files(&self, range: CommitRange) {
        let (gen, sess) = {
            let st = self.shared.lock();
            match &st.session {
                Some(sess) => (st.gen, sess.clone()),
                None => return,
            }
        };
        spawn_range_files(&self.shared, gen, sess, range);
    }

    /// Builds `file`'s patch in `range` (answered by `RangePatch`).
    pub fn request_range_patch(&self, range: CommitRange, file: DiffFile) {
        let (gen, sess) = {
            let st = self.shared.lock();
            match &st.session {
                Some(sess) => (st.gen, sess.clone()),
                None => return,
            }
        };
        spawn_range_patch(&self.shared, gen, sess, range, file);
    }

    /// The file on screen and its first visible row; a highlight of that file starts at
    /// that row's hunk. Cheap (one small mutex, not the state lock): call it every frame.
    pub fn set_visible(&self, path: &str, first_row: usize) {
        let mut v = lock_ok(&self.shared.visible);
        match v.as_mut() {
            Some((p, r)) if p == path => *r = first_row,
            _ => *v = Some((path.to_string(), first_row)),
        }
    }

    /// Asks the head probe again; emits `Stale` if the PR moved since it was fetched. At
    /// most one recheck per open PR is out at a time: a call while one runs does nothing (a
    /// probe still out for a previously open PR does not count). Returns whether a recheck
    /// started.
    pub fn recheck(&self) -> bool {
        let (gen, sess) = {
            let st = self.shared.lock();
            match &st.session {
                Some(sess) => (st.gen, sess.clone()),
                None => return false,
            }
        };
        {
            let mut out = lock_ok(&self.rechecking);
            if *out == Some(gen) {
                return false;
            }
            *out = Some(gen);
        }
        let done = RecheckDone(self.rechecking.clone(), gen);
        let s = self.shared.clone();
        std::thread::Builder::new()
            .name("review-recheck".into())
            .spawn(move || {
                let _done = done;
                check_stale(&s, gen, &sess);
            })
            .is_ok()
    }

    /// Adds a task to the open PR (answered by `TaskCreated`); a no-op until the PR is open.
    pub fn create_task(&self, content: String) {
        let st = self.shared.lock();
        let Some(sess) = st.session.clone() else {
            return;
        };
        let gen = st.gen;
        drop(st);
        spawn_create_task(&self.shared, gen, sess, content);
    }

    /// Fetches the comments again ("Refresh comments"); a no-op until the PR is open.
    pub fn refresh_comments(&self) {
        let st = self.shared.lock();
        let Some(sess) = st.session.clone() else {
            return;
        };
        let gen = st.gen;
        drop(st);
        spawn_comment_fetch(&self.shared, gen, sess);
    }

    /// Rows of a file of the open PR for the inline check before posting, with the fetched
    /// head and whether the file is binary. Builds (and caches) the patch when it is not
    /// cached; call it from a worker thread. A path outside the diff has no rows.
    pub fn row_source(&self) -> RowSource {
        let s = self.shared.clone();
        Arc::new(move |path: &str| {
            let (sess, cached) = {
                let mut st = s.lock();
                let sess = st
                    .session
                    .clone()
                    .ok_or_else(|| "the pull request is not open".to_string())?;
                (sess, st.cache.get(path).map(|(rows, _)| rows))
            };
            let head = sess.refs.head.clone();
            let merge_base = sess.refs.base.clone();
            let (repo, pr_id) = (sess.repo.clone(), sess.pr_id);
            let none = Arc::new(Vec::new());
            let Some(&i) = sess.index.get(path) else {
                return Ok(FileRows {
                    repo: repo.clone(),
                    pr_id,
                    head,
                    merge_base,
                    rows: none,
                    binary: false,
                });
            };
            if sess.files[i].binary {
                return Ok(FileRows {
                    repo: repo.clone(),
                    pr_id,
                    head,
                    merge_base,
                    rows: none,
                    binary: true,
                });
            }
            let rows = match cached {
                Some(rows) => rows,
                None => {
                    let (rows, _, layout) =
                        std::panic::catch_unwind(AssertUnwindSafe(|| build_patch(&s, &sess, path)))
                            .unwrap_or_else(|_| {
                                Err("internal error while loading the diff".into())
                            })?;
                    let mut st = s.lock();
                    if st.session.as_ref().is_some_and(|x| Arc::ptr_eq(x, &sess)) {
                        st.cache.insert(path.to_string(), rows.clone(), layout);
                    }
                    rows
                }
            };
            Ok(FileRows {
                repo,
                pr_id,
                head,
                merge_base,
                rows,
                binary: false,
            })
        })
    }

    /// Cancels pending work and drops the open PR; no event of it is delivered afterwards.
    pub fn close(&self) {
        self.shared.lock().reset();
        let rx = lock_ok(&self.rx);
        while rx.try_recv().is_ok() {}
    }

    /// Non-blocking: drains events of the current generation.
    pub fn poll(&self) -> Vec<ReviewEvent> {
        let gen = self.shared.lock().gen;
        let rx = lock_ok(&self.rx);
        rx.try_iter()
            .filter(|(g, _)| *g == gen)
            .map(|(_, e)| e)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn is_cached(&self, path: &str) -> bool {
        self.shared.lock().cache.contains(path)
    }

    /// Patches built by workers so far, including ones dropped as stale.
    #[cfg(test)]
    pub(crate) fn patches_built(&self) -> u64 {
        self.shared.built.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.shared.lock().session.is_some()
    }

    /// Most loads that ever ran at the same time.
    #[cfg(test)]
    pub(crate) fn max_concurrent_loads(&self) -> usize {
        self.shared.loads_max.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn live_workers(&self) -> usize {
        self.shared.workers_live.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn set_patch_hook(&self, hook: PatchHook) {
        *self.shared.patch_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_highlight_hook(&self, hook: PatchHook) {
        *self.shared.hl_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_load_hook(&self, hook: PatchHook) {
        *self.shared.load_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_batch_hook(&self, hook: PatchHook) {
        *self.shared.batch_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_counts_hook(&self, hook: PatchHook) {
        *self.shared.counts_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_locator(&self, locator: Locator) {
        *self.shared.locator.lock().unwrap() = Some(locator);
    }

    /// Runs git through `git` (its own spawn counter) instead of the shared one.
    #[cfg(test)]
    pub(super) fn use_git(&self, git: Git) {
        assert!(self.shared.git.set(git).is_ok(), "git already in use");
    }

    #[cfg(test)]
    pub(super) fn set_whole_pr_lines(&self, lines: u64) {
        self.shared.whole_pr_lines.store(lines, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn set_cache_cap(&self, bytes: usize) {
        self.shared.lock().cache = PatchCache::new(bytes);
    }

    #[cfg(test)]
    pub(super) fn cache_bytes(&self) -> usize {
        self.shared.lock().cache.bytes()
    }

    #[cfg(test)]
    pub(super) fn batches_run(&self) -> u64 {
        self.shared.batches_run.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn batches_dropped(&self) -> u64 {
        self.shared.batches_dropped.load(Ordering::Relaxed)
    }
}

impl Drop for ReviewService {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.reset();
        st.shutdown = true;
        drop(st);
        self.shared.work.notify_all();
    }
}
