use super::*;

pub(super) struct Shared {
    pub(super) state: Mutex<State>,
    pub(super) work: Condvar,
    pub(super) tx: Sender<(u64, ReviewEvent)>,
    pub(super) ctx: egui::Context,
    pub(super) store: Arc<Mutex<Store>>,
    /// Credentials, head probe and comment fetcher, asked for at each open.
    pub(super) deps: DepsFn,
    /// Latest comment fetch started; an older fetch finishing later is dropped.
    pub(super) comments_seq: AtomicU64,
    pub(super) source: Source,
    pub(super) git: OnceLock<Git>,
    /// Whether an open PR this small (changed lines) is prefetched whole; 0 never does.
    pub(super) whole_pr_lines: AtomicU64,
    /// Workers in the pool (a single worker does not prefetch the whole PR).
    pub(super) threads: usize,
    /// Per-file patches built by workers (cancellation diagnostics).
    pub(super) built: AtomicU64,
    /// Batches run to the end, and of those the ones dropped as stale (diagnostics).
    pub(super) batches_run: AtomicU64,
    pub(super) batches_dropped: AtomicU64,
    /// Loads running now / most ever running at once (serialization diagnostics).
    pub(super) loads_active: AtomicUsize,
    pub(super) loads_max: AtomicUsize,
    /// Serializes this service's loads (a superseded load waiting here does no git work);
    /// `repo_load_lock` additionally serializes loads of one repository across services.
    pub(super) load_lock: Mutex<()>,
    /// Loads waiting for (or holding) the repository lock (tests).
    #[cfg(test)]
    pub(super) load_waiters: AtomicUsize,
    /// Test hook run at the start of each load, under both locks.
    #[cfg(test)]
    pub(super) load_hook: Mutex<Option<PatchHook>>,
    /// Worker threads currently alive.
    pub(super) workers_live: AtomicUsize,
    /// The file on screen and its first visible row (from the UI, every frame it changes):
    /// a highlight starts at that row's hunk.
    pub(super) visible: Mutex<Option<(String, usize)>>,
    /// Wall time one file's highlight may take (`HIGHLIGHT_BUDGET`; tests lower it).
    pub(super) hl_budget_ms: AtomicU64,
    /// Highlight jobs ended, however (diagnostics, tests).
    pub(super) hl_ended: AtomicU64,
    /// Test hook run before each patch is built (e.g. to inject a panic).
    #[cfg(test)]
    pub(super) patch_hook: Mutex<Option<PatchHook>>,
    /// Test hook run before each highlight job (e.g. to inject a panic).
    #[cfg(test)]
    pub(super) hl_hook: Mutex<Option<PatchHook>>,
    /// Test hook run inside each batch before git (gets "whole" or "near").
    #[cfg(test)]
    pub(super) batch_hook: Mutex<Option<PatchHook>>,
    /// Test hook run on the numstat thread before git.
    #[cfg(test)]
    pub(super) counts_hook: Mutex<Option<PatchHook>>,
    /// Every highlight job that ended: its path and when.
    #[cfg(test)]
    pub(super) hl_done: Mutex<Vec<(String, Instant)>>,
    /// Test replacement for `Git::shared` (the git lookup).
    #[cfg(test)]
    pub(super) locator: Mutex<Option<Locator>>,
}

#[cfg(test)]
pub(super) type PatchHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
pub(super) type Locator = Arc<dyn Fn() -> Result<Git, GitError> + Send + Sync>;

impl Shared {
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        lock_ok(&self.state)
    }

    /// Queues an event without waking the UI; for use under the state lock (the caller
    /// repaints after releasing it, so the state lock never wraps egui's context lock).
    pub(super) fn send(&self, gen: u64, ev: ReviewEvent) {
        let _ = self.tx.send((gen, ev));
    }

    pub(super) fn emit(&self, gen: u64, ev: ReviewEvent) {
        self.send(gen, ev);
        self.ctx.request_repaint();
    }

    pub(super) fn current(&self, gen: u64) -> bool {
        self.lock().gen == gen
    }

    /// The first row on screen when `path` is the file shown.
    pub(super) fn visible_row(&self, path: &str) -> Option<usize> {
        let v = lock_ok(&self.visible);
        v.as_ref().filter(|(p, _)| p == path).map(|(_, r)| *r)
    }

    /// The process-wide located git (found once, by the start-up check or the first open).
    /// Only a found git is kept: a failed lookup (a transient `git --version` failure, or git
    /// installed later) is tried again at the next call.
    pub(super) fn git(&self) -> Result<&Git, String> {
        if let Some(git) = self.git.get() {
            return Ok(git);
        }
        let found = self.locate().map_err(|e| e.to_string())?;
        Ok(self.git.get_or_init(|| found))
    }

    pub(super) fn locate(&self) -> Result<Git, GitError> {
        #[cfg(test)]
        {
            let locator = lock_ok(&self.locator).clone();
            if let Some(l) = locator {
                return l();
            }
        }
        Git::shared()
    }

    /// Credentials for lazy blob fetches from a blobless mirror; none for the local path.
    pub(super) fn patch_auth<'a>(&self, sess: &'a Session) -> Option<&'a GitAuth> {
        match self.source {
            Source::Remote(_) => sess.deps.auth.as_ref(),
            Source::Local(_) => None,
        }
    }

    /// Identifies the local copy a load works on: the repository path for the local
    /// source, the remote repository otherwise (its clone or mirror follows from it).
    pub(super) fn load_key(&self, repo: &RepoRef) -> String {
        match &self.source {
            Source::Local(path) => format!("local:{}", path.display()),
            Source::Remote(_) => format!("remote:{}", repo.full_name().to_lowercase()),
        }
    }

    pub(super) fn clones_for(&self, git: &Git, root: Option<&Path>, url: &str) -> Vec<LocalClone> {
        let Some(root) = root else {
            return Vec::new();
        };
        clones_in(clone_cache(), git, root, url, CLONE_RESCAN_EVERY)
    }
}
