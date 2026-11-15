//! Opens a PR against local git on std threads and serves parsed per-file patches.
//!
//! Nothing here runs on the egui thread except `open`/`request_patch`/`close`/`poll`,
//! which only touch a mutex and a channel. Every event carries the generation it was
//! produced for; `open` and `close` bump the generation, so late results are dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use gitlocal::{
    batch_patches_cancellable, discover_clones, fetch_pr, file_counts, file_list, file_patch,
    find_clone, needs_clone, pick_local, ref_prefix, resolve_commits, FetchSpec, Git, GitAuth,
    GitError, GitProgress, LocalClone, LocalRepo, PrRefs, RepoKind,
};
use prcore::comments::{build_threads, Thread};
use prcore::commit::same_commit_or_unknown;
use prcore::diff::{apply_counts, parse_patch, DiffFile, FileCounts, Row, RowKind};
use prcore::model::{Forge, PrState, PullRequest, RepoRef};
use providers::provider::CommentList;
use store::Store;

use super::cache::{PatchCache, PATCH_CACHE_BYTES};
use super::composer::{FileRows, RowSource};
use super::diffview::PatchLayout;
use super::highlight::{
    highlight_chunked, Chunking, HighlightQueue, HlEnd, HlJob, Next, RowSpans, Spans, CHUNK_ROWS,
    HIGHLIGHT_BUDGET,
};
use crate::lock::lock_ok;
use crate::settings_ui::default_download_dir;

/// Files with more changed lines than this render plain (no highlight, no word diff).
pub const PLAIN_CHANGED_LINES: u32 = 20_000;

/// Files prefetched on each side of a requested file, in index order.
pub const PREFETCH_RADIUS: usize = 5;

/// How deep below `clone_root` existing clones are searched for.
const CLONE_SCAN_DEPTH: usize = 4;

/// Queued (not started) priority requests kept; older ones are dropped, so holding j/k
/// does not build a patch for every file passed on the way.
const KEEP_PRIORITY: usize = 3;

/// Queued (not started) neighbour batches kept, newest first; older ones are dropped.
const KEEP_BATCHES: usize = 2;

/// A PR with at most this many changed lines in total (and no plain file) is prefetched
/// whole with one `git diff` once it is open.
pub const WHOLE_PR_LINES: u64 = 20_000;

/// Progress shown before the first clone of a repository's mirror.
pub const CLONE_PHASE: &str =
    "Cloning the repository for the first time (can take several minutes for large repos)...";
/// The clone phase once git reports progress: "receiving objects 45%".
fn clone_progress_phase(p: &GitProgress) -> String {
    format!(
        "Cloning the repository for the first time: {} {}%",
        p.phase.to_lowercase(),
        p.percent
    )
}

pub const FETCH_PHASE: &str = "Fetching branches...";
/// Listing the files of a blobless mirror downloads the file contents it needs.
pub const MIRROR_FILES_PHASE: &str = "Reading changed files (downloading file contents)...";
pub const LIST_PHASE: &str = "Listing changed files...";

fn files_phase(kind: RepoKind) -> &'static str {
    match kind {
        RepoKind::Mirror => MIRROR_FILES_PHASE,
        RepoKind::Clone => LIST_PHASE,
    }
}

/// Returns the PR's current source commit as the server sees it (`Provider::pr()`).
pub type HeadProbe = Arc<dyn Fn(&RepoRef, u64) -> Result<String, String> + Send + Sync>;

/// Returns every comment of the PR (`Provider::comments()`); errors are category text.
pub type CommentFetcher = Arc<dyn Fn(&RepoRef, u64) -> Result<CommentList, String> + Send + Sync>;

type UrlFor = Arc<dyn Fn(&RepoRef) -> String + Send + Sync>;

/// What one open needs from the signed-in user: git credentials, the head probe and the
/// comment fetcher. Built on the open's own thread (credential reads never block the UI).
pub struct ReviewDeps {
    pub auth: Option<GitAuth>,
    pub probe: HeadProbe,
    pub comments: CommentFetcher,
}

/// Called once per open, on the open's thread, with the repository opened (its account's
/// credentials and provider).
pub type DepsFn = Arc<dyn Fn(&RepoRef) -> Arc<ReviewDeps> + Send + Sync>;

fn fixed_deps(auth: Option<GitAuth>, probe: HeadProbe, comments: CommentFetcher) -> DepsFn {
    let deps = Arc::new(ReviewDeps {
        auth,
        probe,
        comments,
    });
    Arc::new(move |_: &RepoRef| deps.clone())
}

/// Shortest time between two automatic rescans of `clone_root` (when no clone matched).
const CLONE_RESCAN_EVERY: Duration = Duration::from_secs(10);

/// One scan of `clone_root`.
struct CloneScan {
    root: PathBuf,
    found: Vec<LocalClone>,
    at: Instant,
}

/// Last `clone_root` scan in this process. Shared by every review and replaced by a rescan
/// in Settings; a different root scans again.
fn clone_cache() -> &'static Mutex<Option<CloneScan>> {
    static CACHE: Mutex<Option<CloneScan>> = Mutex::new(None);
    &CACHE
}

/// Records a fresh scan of `root` (Settings' rescan), so the next review uses it.
pub fn remember_clones(root: &Path, found: &[LocalClone]) {
    remember_in(clone_cache(), root, found);
}

fn remember_in(cache: &Mutex<Option<CloneScan>>, root: &Path, found: &[LocalClone]) {
    *lock_ok(cache) = Some(CloneScan {
        root: root.to_path_buf(),
        found: found.to_vec(),
        at: Instant::now(),
    });
}

/// The clones of `root` from `cache`. Entries whose directory is gone are dropped. `root` is
/// walked on first use, for another root, and once more when no cached clone matches `url`
/// and the last walk is at least `rescan_every` old (a clone made since then is found; a
/// repository with no clone does not walk the tree at every open).
fn clones_in(
    cache: &Mutex<Option<CloneScan>>,
    git: &Git,
    root: &Path,
    url: &str,
    rescan_every: Duration,
) -> Vec<LocalClone> {
    {
        let mut slot = lock_ok(cache);
        if let Some(scan) = slot.as_mut().filter(|s| s.root == root) {
            scan.found.retain(|c| c.path.is_dir());
            if find_clone(&scan.found, url).is_some() || scan.at.elapsed() < rescan_every {
                return scan.found.clone();
            }
        }
    }
    // scan without holding the lock; a concurrent scan of the same root just repeats work
    let (found, _skipped) = discover_clones(git, root, CLONE_SCAN_DEPTH);
    remember_in(cache, root, &found);
    found
}

/// Drops every cached clone at `path` (it failed as a local copy).
fn forget_clone_in(cache: &Mutex<Option<CloneScan>>, path: &Path) {
    if let Some(scan) = lock_ok(cache).as_mut() {
        scan.found.retain(|c| c.path != path);
    }
}

/// Whether a failed fetch into `local` should be retried once on another local copy: the
/// copy is a discovered clone that git could not even run in (an i/o error, or the directory
/// is gone). A mirror, or git failing inside the clone (auth, network), is not retried.
fn retry_on_other_copy(local: &LocalRepo, err: &GitError) -> bool {
    local.kind == RepoKind::Clone
        && (matches!(err, GitError::Io(_) | GitError::NotInstalled) || !local.path.is_dir())
}

#[derive(Clone, Debug)]
pub enum ReviewEvent {
    /// Progress text, e.g. "Fetching branches...".
    Phase(String),
    /// The file index, sorted by path, as soon as `git diff --raw` listed it: names, status
    /// and renames are final; the line counts and the binary flag are unknown
    /// (`counts_known` false) until `Counts`.
    Opened { refs: PrRefs, files: Vec<DiffFile> },
    /// Line counts and binary flags of the files of `Opened` (from `--numstat`, which ran
    /// in parallel with `--raw`), by path.
    Counts { counts: Arc<Vec<FileCounts>> },
    /// `--numstat` failed: counts stay unknown; the file list and diffs are unaffected.
    CountsFailed(String),
    /// `plain`: over `PLAIN_CHANGED_LINES` changed lines, render without highlighting.
    /// `layout` was computed with the rows, on the worker.
    Patch {
        path: String,
        rows: Arc<Vec<Row>>,
        plain: bool,
        layout: Arc<PatchLayout>,
    },
    /// Syntax colours for rows `range` of a file of `rows` rows (`spans` has one entry per
    /// row of `range`), sent after its `Patch` (never for `plain` files). A file's colours
    /// come in steps, the rows on screen first; remembered ones as one `0..rows` event.
    Highlighted {
        path: String,
        rows: usize,
        range: Range<usize>,
        spans: Arc<Spans>,
    },
    /// One file's patch could not be produced; the rest of the PR is unaffected.
    PatchFailed { path: String, error: String },
    /// Opening the PR failed; category text only, never secrets.
    Failed(String),
    /// The server's source commit differs from the fetched head: reload needed.
    Stale,
    /// The PR's comment threads (after `Opened`, and on `refresh_comments`); `skipped`
    /// counts comments the provider could not decode.
    Comments {
        threads: Vec<Thread>,
        skipped: usize,
    },
    /// Fetching comments failed; the diff is unaffected.
    CommentsFailed(String),
}

/// `https://<host>/<namespace>/<slug>.git` on the repository's own host (`bitbucket.org`,
/// `github.com`, a GitHub Enterprise host, a GitLab host with its relative URL root), with
/// no userinfo.
pub(crate) fn https_url(repo: &RepoRef) -> String {
    format!("https://{}/{}/{}.git", repo.host, repo.namespace, repo.slug)
}

/// `url` over plain http when `auth` is a cleartext account's (a private address): the
/// remote is then on exactly the account's http origin. Any other url is left as is.
pub(crate) fn remote_url(url: String, auth: &GitAuth) -> String {
    match url.strip_prefix("https://") {
        Some(rest) if auth.scheme() == "http" => format!("http://{rest}"),
        _ => url,
    }
}

/// Fork PRs stay unsupported on Bitbucket; GitHub and GitLab fetch them from the base
/// repository's PR ref (decision D2).
fn refuses_fork(pr: &PullRequest) -> bool {
    pr.is_fork() && pr.forge == Forge::BitbucketCloud
}

/// Over the plain limit by its numstat counts (false while they are unknown). Used only to
/// plan prefetching; whether a built patch renders plain is `plain_rows`.
fn is_plain(f: &DiffFile) -> bool {
    u64::from(f.additions) + u64::from(f.deletions) > u64::from(PLAIN_CHANGED_LINES)
}

/// Over the plain limit by its own rows: the added and deleted rows are exactly what
/// numstat counts for the same diff, and they are known without waiting for numstat.
fn plain_rows(rows: &[Row]) -> bool {
    let changed = rows
        .iter()
        .filter(|r| matches!(r.kind, RowKind::Add | RowKind::Del))
        .count();
    changed > PLAIN_CHANGED_LINES as usize
}

enum Source {
    /// Real path: pick a clone or mirror, fetch the PR over HTTPS.
    Remote(UrlFor),
    /// Demo/test path: a repository that already has both branches.
    Local(PathBuf),
}

/// Replaced by a copy with the counts filled in when numstat arrives (`files` changes, the
/// rest stays).
#[derive(Clone)]
struct Session {
    repo: RepoRef,
    pr_id: u64,
    local: LocalRepo,
    refs: PrRefs,
    files: Vec<DiffFile>,
    index: HashMap<String, usize>,
    deps: Arc<ReviewDeps>,
}

/// Speculative patches for several files from one `git diff`; fills the cache only.
struct Batch {
    paths: Vec<String>,
    /// The whole PR (lowest priority; its paths are not reserved in `batching`).
    whole_pr: bool,
}

struct Job {
    path: String,
    /// Requested by the UI (emit an event); false for prefetch (cache only).
    wanted: bool,
    /// Queued as the selected file (front of the queue); only the newest few are kept.
    priority: bool,
}

struct State {
    gen: u64,
    session: Option<Arc<Session>>,
    queue: VecDeque<Job>,
    queued: HashSet<String>,
    /// path -> wanted
    in_flight: HashMap<String, bool>,
    cache: PatchCache,
    /// Highlight jobs; workers take them only when no patch is waiting.
    hl: HighlightQueue,
    /// Batch jobs, taken only when no patch (and, by highlighting workers, no highlight) waits.
    batches: VecDeque<Batch>,
    /// Paths of queued or running neighbour batches (not asked for again meanwhile).
    batching: HashSet<String>,
    /// Batches running now, of any generation (at most `batch_slots`).
    batches_running: usize,
    /// The file last served to the screen: highlights of files outside its prefetch window
    /// are abandoned.
    focus: Option<String>,
    /// `gen`, readable without the lock: a running highlight checks it before every row.
    gen_now: Arc<AtomicU64>,
    shutdown: bool,
}

/// The next piece of work of a worker.
enum Task {
    Patch {
        gen: u64,
        sess: Arc<Session>,
        path: String,
    },
    Highlight {
        gen: u64,
        gen_now: Arc<AtomicU64>,
        job: HlJob,
    },
    Batch {
        gen: u64,
        sess: Arc<Session>,
        batch: Batch,
    },
}

impl State {
    fn new() -> State {
        State {
            gen: 0,
            session: None,
            queue: VecDeque::new(),
            queued: HashSet::new(),
            in_flight: HashMap::new(),
            cache: PatchCache::new(PATCH_CACHE_BYTES),
            hl: HighlightQueue::default(),
            batches: VecDeque::new(),
            batching: HashSet::new(),
            batches_running: 0,
            focus: None,
            gen_now: Arc::default(),
            shutdown: false,
        }
    }

    fn reset(&mut self) {
        self.hl.clear();
        self.gen += 1;
        self.gen_now.store(self.gen, Ordering::SeqCst);
        self.focus = None;
        self.session = None;
        self.queue.clear();
        self.queued.clear();
        self.in_flight.clear();
        self.cache.clear();
        self.batches.clear();
        self.batching.clear();
    }

    /// Whether `path` is cached, queued, being built or part of a pending neighbour batch.
    fn busy(&self, path: &str) -> bool {
        self.cache.contains(path)
            || self.queued.contains(path)
            || self.in_flight.contains_key(path)
            || self.batching.contains(path)
    }

    /// Queues a cache-only per-file job (the fallback when a batch cannot serve a file).
    fn prefetch_one(&mut self, path: &str) {
        if !self.busy(path) {
            self.queued.insert(path.to_string());
            self.queue.push_back(Job {
                path: path.to_string(),
                wanted: false,
                priority: false,
            });
        }
    }

    fn enqueue(&mut self, path: String, wanted: bool, priority: bool) {
        if let Some(w) = self.in_flight.get_mut(&path) {
            *w |= wanted;
            return;
        }
        if self.queued.contains(&path) {
            let Some(pos) = self.queue.iter().position(|j| j.path == path) else {
                return;
            };
            self.queue[pos].wanted |= wanted;
            if priority {
                let mut job = self.queue.remove(pos).expect("position is in range");
                job.priority = true;
                self.queue.push_front(job);
                self.drop_old_priority();
            }
            return;
        }
        self.queued.insert(path.clone());
        let job = Job {
            path,
            wanted,
            priority,
        };
        if priority {
            self.queue.push_front(job);
            self.drop_old_priority();
        } else {
            self.queue.push_back(job);
        }
    }

    /// Keeps the newest `KEEP_PRIORITY` queued priority requests (the front of the queue)
    /// and drops the others: files passed while j/k is held. Selecting one again requests it
    /// again.
    fn drop_old_priority(&mut self) {
        let mut seen = 0;
        let queued = &mut self.queued;
        self.queue.retain(|j| {
            if !j.priority {
                return true;
            }
            seen += 1;
            let keep = seen <= KEEP_PRIORITY;
            if !keep {
                queued.remove(&j.path);
            }
            keep
        });
    }

    /// Prefetches up to `PREFETCH_RADIUS` files on each side of `path`, nearest first: the
    /// ordinary ones as one batch (one git process), plain (large) ones one by one.
    fn prefetch_around(&mut self, sess: &Session, path: &str) {
        let Some(&i) = sess.index.get(path) else {
            return;
        };
        let mut near = Vec::with_capacity(2 * PREFETCH_RADIUS);
        for d in 1..=PREFETCH_RADIUS {
            if let Some(f) = sess.files.get(i + d) {
                near.push(f);
            }
            if let Some(j) = i.checked_sub(d) {
                near.push(&sess.files[j]);
            }
        }
        let mut batch = Vec::new();
        for f in near {
            if self.busy(&f.path) {
                continue;
            }
            if is_plain(f) {
                self.prefetch_one(&f.path);
            } else {
                batch.push(f.path.clone());
            }
        }
        match batch.len() {
            0 => {}
            1 => self.prefetch_one(&batch[0]),
            _ => self.push_neighbour_batch(batch),
        }
    }

    /// Queues a neighbour batch ahead of older ones; keeps the newest `KEEP_BATCHES` (and a
    /// whole-PR batch, which always stays last).
    fn push_neighbour_batch(&mut self, paths: Vec<String>) {
        self.batching.extend(paths.iter().cloned());
        self.batches.push_front(Batch {
            paths,
            whole_pr: false,
        });
        let mut seen = 0;
        let batching = &mut self.batching;
        self.batches.retain(|b| {
            if b.whole_pr {
                return true;
            }
            seen += 1;
            let keep = seen <= KEEP_BATCHES;
            if !keep {
                for p in &b.paths {
                    batching.remove(p);
                }
            }
            keep
        });
    }

    /// Takes the next batch, dropping paths cached or being built meanwhile.
    fn pop_batch(&mut self) -> Option<Batch> {
        let mut b = self.batches.pop_front()?;
        let whole = b.whole_pr;
        let (cache, in_flight, batching) = (&self.cache, &self.in_flight, &mut self.batching);
        b.paths.retain(|p| {
            let keep = !cache.contains(p) && !in_flight.contains_key(p);
            if !keep && !whole {
                batching.remove(p);
            }
            keep
        });
        Some(b)
    }

    /// The patch job at `pos` of the queue, marked in flight.
    fn patch_task(&mut self, pos: usize, sess: Arc<Session>) -> Option<Task> {
        let job = self.queue.remove(pos)?;
        self.queued.remove(&job.path);
        self.in_flight.insert(job.path.clone(), job.wanted);
        Some(Task::Patch {
            gen: self.gen,
            sess,
            path: job.path,
        })
    }

    fn highlight_task(&self, job: HlJob) -> Task {
        Task::Highlight {
            gen: self.gen,
            gen_now: self.gen_now.clone(),
            job,
        }
    }

    /// The next job of a worker, most wanted first: a patch the screen asked for, the
    /// colours of the file on screen, a prefetch patch, the colours of a prefetched file,
    /// a batch (at most `batch_slots` at once). The reserved worker takes patches only.
    fn take_task(&mut self, reserved: bool, batch_slots: usize) -> Option<Task> {
        let sess = self.session.clone();
        if let Some(sess) = &sess {
            if let Some(pos) = self.queue.iter().position(|j| j.wanted) {
                return self.patch_task(pos, sess.clone());
            }
        }
        if !reserved {
            if let Some(job) = self.hl.pop() {
                return Some(self.highlight_task(job));
            }
        }
        if let Some(sess) = &sess {
            if !self.queue.is_empty() {
                return self.patch_task(0, sess.clone());
            }
        }
        if reserved {
            return None;
        }
        if let Some(job) = self.hl.pop_background() {
            return Some(self.highlight_task(job));
        }
        if let Some(sess) = sess.filter(|_| self.batches_running < batch_slots) {
            if let Some(batch) = self.pop_batch() {
                self.batches_running += 1;
                return Some(Task::Batch {
                    gen: self.gen,
                    sess,
                    batch,
                });
            }
        }
        None
    }

    /// `path` is the file on screen or within `PREFETCH_RADIUS` files of it (in index
    /// order); with nothing on screen yet, none is.
    fn near_focus(&self, path: &str) -> bool {
        let (Some(focus), Some(sess)) = (self.focus.as_deref(), self.session.as_ref()) else {
            return false;
        };
        if focus == path {
            return true;
        }
        match (sess.index.get(focus), sess.index.get(path)) {
            (Some(&a), Some(&b)) => a.abs_diff(b) <= PREFETCH_RADIUS,
            _ => false,
        }
    }

    /// A prefetched patch went into the cache: its colours are made in the background when
    /// it is near the file on screen.
    fn prefetched(&mut self, path: &str, rows: &Arc<Vec<Row>>, plain: bool) {
        if !plain && self.near_focus(path) {
            self.hl.request_background(path, rows);
        }
    }
}

/// Whether `files` is small enough to prefetch whole: at most `limit` changed lines in
/// total and no plain file.
fn whole_pr_eligible(files: &[DiffFile], limit: u64) -> bool {
    let total: u64 = files
        .iter()
        .map(|f| u64::from(f.additions) + u64::from(f.deletions))
        .sum();
    !files.is_empty()
        && files.iter().all(|f| f.counts_known)
        && total <= limit
        && !files.iter().any(is_plain)
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    tx: Sender<(u64, ReviewEvent)>,
    ctx: egui::Context,
    store: Arc<Mutex<Store>>,
    /// Credentials, head probe and comment fetcher, asked for at each open.
    deps: DepsFn,
    /// Latest comment fetch started; an older fetch finishing later is dropped.
    comments_seq: AtomicU64,
    source: Source,
    git: OnceLock<Git>,
    /// Whether an open PR this small (changed lines) is prefetched whole; 0 never does.
    whole_pr_lines: AtomicU64,
    /// Workers in the pool (a single worker does not prefetch the whole PR).
    threads: usize,
    /// Per-file patches built by workers (cancellation diagnostics).
    built: AtomicU64,
    /// Batches run to the end, and of those the ones dropped as stale (diagnostics).
    batches_run: AtomicU64,
    batches_dropped: AtomicU64,
    /// Loads running now / most ever running at once (serialization diagnostics).
    loads_active: AtomicUsize,
    loads_max: AtomicUsize,
    /// Serializes this service's loads (a superseded load waiting here does no git work);
    /// `repo_load_lock` additionally serializes loads of one repository across services.
    load_lock: Mutex<()>,
    /// Loads waiting for (or holding) the repository lock (tests).
    #[cfg(test)]
    load_waiters: AtomicUsize,
    /// Test hook run at the start of each load, under both locks.
    #[cfg(test)]
    load_hook: Mutex<Option<PatchHook>>,
    /// Worker threads currently alive.
    workers_live: AtomicUsize,
    /// The file on screen and its first visible row (from the UI, every frame it changes):
    /// a highlight starts at that row's hunk.
    visible: Mutex<Option<(String, usize)>>,
    /// Wall time one file's highlight may take (`HIGHLIGHT_BUDGET`; tests lower it).
    hl_budget_ms: AtomicU64,
    /// Highlight jobs ended, however (diagnostics, tests).
    hl_ended: AtomicU64,
    /// Test hook run before each patch is built (e.g. to inject a panic).
    #[cfg(test)]
    patch_hook: Mutex<Option<PatchHook>>,
    /// Test hook run before each highlight job (e.g. to inject a panic).
    #[cfg(test)]
    hl_hook: Mutex<Option<PatchHook>>,
    /// Test hook run inside each batch before git (gets "whole" or "near").
    #[cfg(test)]
    batch_hook: Mutex<Option<PatchHook>>,
    /// Test hook run on the numstat thread before git.
    #[cfg(test)]
    counts_hook: Mutex<Option<PatchHook>>,
    /// Every highlight job that ended: its path and when.
    #[cfg(test)]
    hl_done: Mutex<Vec<(String, Instant)>>,
    /// Test replacement for `Git::shared` (the git lookup).
    #[cfg(test)]
    locator: Mutex<Option<Locator>>,
}

#[cfg(test)]
type PatchHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
type Locator = Arc<dyn Fn() -> Result<Git, GitError> + Send + Sync>;

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock_ok(&self.state)
    }

    /// Queues an event without waking the UI; for use under the state lock (the caller
    /// repaints after releasing it, so the state lock never wraps egui's context lock).
    fn send(&self, gen: u64, ev: ReviewEvent) {
        let _ = self.tx.send((gen, ev));
    }

    fn emit(&self, gen: u64, ev: ReviewEvent) {
        self.send(gen, ev);
        self.ctx.request_repaint();
    }

    fn current(&self, gen: u64) -> bool {
        self.lock().gen == gen
    }

    /// The first row on screen when `path` is the file shown.
    fn visible_row(&self, path: &str) -> Option<usize> {
        let v = lock_ok(&self.visible);
        v.as_ref().filter(|(p, _)| p == path).map(|(_, r)| *r)
    }

    /// The process-wide located git (found once, by the start-up check or the first open).
    /// Only a found git is kept: a failed lookup (a transient `git --version` failure, or git
    /// installed later) is tried again at the next call.
    fn git(&self) -> Result<&Git, String> {
        if let Some(git) = self.git.get() {
            return Ok(git);
        }
        let found = self.locate().map_err(|e| e.to_string())?;
        Ok(self.git.get_or_init(|| found))
    }

    fn locate(&self) -> Result<Git, GitError> {
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
    fn patch_auth<'a>(&self, sess: &'a Session) -> Option<&'a GitAuth> {
        match self.source {
            Source::Remote(_) => sess.deps.auth.as_ref(),
            Source::Local(_) => None,
        }
    }

    /// Identifies the local copy a load works on: the repository path for the local
    /// source, the remote repository otherwise (its clone or mirror follows from it).
    fn load_key(&self, repo: &RepoRef) -> String {
        match &self.source {
            Source::Local(path) => format!("local:{}", path.display()),
            Source::Remote(_) => format!("remote:{}", repo.full_name().to_lowercase()),
        }
    }

    fn clones_for(&self, git: &Git, root: Option<&Path>, url: &str) -> Vec<LocalClone> {
        let Some(root) = root else {
            return Vec::new();
        };
        clones_in(clone_cache(), git, root, url, CLONE_RESCAN_EVERY)
    }
}

/// Reads both branch tips from a repository that has them and computes the merge-base.
/// What to fetch for `pr` (spec 7.1): Bitbucket fetches both branches by name; GitHub and
/// GitLab fetch the forge's own PR ref (it survives a deleted or force-pushed source branch
/// and covers fork PRs) and the destination branch.
fn fetch_spec(pr: &PullRequest) -> FetchSpec {
    let src_ref = match pr.forge {
        Forge::BitbucketCloud => {
            return FetchSpec::Branches {
                src: pr.source_branch.clone(),
                dst: pr.dest_branch.clone(),
            }
        }
        Forge::GitHub => format!("refs/pull/{}/head", pr.id),
        Forge::GitLab => format!("refs/merge-requests/{}/head", pr.id),
    };
    FetchSpec::PullRef {
        src_ref,
        dst: pr.dest_branch.clone(),
    }
}

/// `fetch(fetch_spec(pr))`, and for a closed or merged GitLab MR (not a fork) whose
/// `refs/merge-requests/<iid>/head` is gone (GitLab deletes it 14 days after close or merge,
/// spec 7.1) once more by its source branch.
fn fetch_with_fallback<T, E>(
    pr: &PullRequest,
    fetch: impl Fn(&FetchSpec) -> Result<T, E>,
) -> Result<T, E> {
    let first = fetch(&fetch_spec(pr));
    let fallback = pr.forge == Forge::GitLab && pr.state != PrState::Open && !pr.is_fork();
    match first {
        Err(_) if fallback => fetch(&FetchSpec::Branches {
            src: pr.source_branch.clone(),
            dst: pr.dest_branch.clone(),
        }),
        r => r,
    }
}

fn local_refs(git: &Git, local: &LocalRepo, pr: &PullRequest) -> Result<PrRefs, GitError> {
    let src = format!("refs/heads/{}", pr.source_branch);
    let dst = format!("refs/heads/{}", pr.dest_branch);
    let [head, dest] = resolve_commits(git, local, [&src, &dst])?;
    let base = match git.run(&local.path, &["merge-base", &dest, &head], None) {
        Ok(out) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(GitError::Failed { code: Some(1), .. }) => return Err(GitError::NoMergeBase),
        Err(e) => return Err(e),
    };
    Ok(PrRefs { base, head, dest })
}

const CANCELLED: &str = "cancelled";

/// One load at a time per local copy in the whole process, like `gitlocal`'s mirror lock:
/// opening, going back and reopening (or a second review service) never run two
/// `fetch_pr` / `file_index` on the same repository at once.
fn repo_load_lock(key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut map = lock_ok(LOCKS.get_or_init(Default::default));
    map.entry(key.to_string()).or_default().clone()
}

/// Counts concurrent loads for the lifetime of the value.
struct LoadCount<'a>(&'a Shared);

impl<'a> LoadCount<'a> {
    fn start(s: &'a Shared) -> LoadCount<'a> {
        let now = s.loads_active.fetch_add(1, Ordering::SeqCst) + 1;
        s.loads_max.fetch_max(now, Ordering::SeqCst);
        LoadCount(s)
    }
}

impl Drop for LoadCount<'_> {
    fn drop(&mut self) {
        self.0.loads_active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn load(
    s: &Arc<Shared>,
    gen: u64,
    repo: &RepoRef,
    pr: &PullRequest,
    deps: Arc<ReviewDeps>,
) -> Result<(Session, PendingCounts), String> {
    let _count = LoadCount::start(s);
    #[cfg(test)]
    {
        let hook = lock_ok(&s.load_hook).clone();
        if let Some(h) = hook {
            h(&repo.full_name());
        }
    }
    let phase = |p: &str| s.emit(gen, ReviewEvent::Phase(p.into()));
    if refuses_fork(pr) {
        return Err("Pull requests from forks are not supported".into());
    }
    phase("Locating repository...");
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    let (local, refs) = match &s.source {
        Source::Local(path) => {
            let local = LocalRepo {
                path: path.clone(),
                kind: RepoKind::Clone,
            };
            phase(FETCH_PHASE);
            let refs = local_refs(git, &local, pr)
                .map_err(|e| format!("Could not read the pull request's branches: {e}"))?;
            (local, refs)
        }
        Source::Remote(url_for) => {
            let Some(auth) = deps.auth.as_ref() else {
                return Err("Sign in to load this pull request's diff".into());
            };
            let url = remote_url(url_for(repo), auth);
            let settings = lock_ok(&s.store)
                .settings(&default_download_dir())
                .map_err(|_| "Could not read the folder settings".to_string())?;
            let root = settings.clone_root.as_deref();
            // git's clone progress, shown only when the phase or percent changes
            let last = std::cell::RefCell::new(None::<GitProgress>);
            let progress = |p: GitProgress| {
                if last.borrow().as_ref() != Some(&p) {
                    phase(&clone_progress_phase(&p));
                    *last.borrow_mut() = Some(p);
                }
            };
            let prepare = |clones: &[LocalClone]| {
                if needs_clone(clones, repo, &settings.download_dir, &url) {
                    phase(CLONE_PHASE);
                }
                pick_local(
                    clones,
                    repo,
                    &settings.download_dir,
                    git,
                    &url,
                    auth,
                    Some(&progress),
                )
                .map_err(|e| format!("Could not prepare a local copy of the repository: {e}"))
            };
            let prefix = ref_prefix(&repo.host);
            let fetch = |local: &LocalRepo| {
                fetch_with_fallback(pr, |spec| {
                    fetch_pr(git, local, &url, auth, &prefix, pr.id, spec)
                })
            };
            let mut local = prepare(&s.clones_for(git, root, &url))?;
            if !s.current(gen) {
                return Err(CANCELLED.into());
            }
            phase(FETCH_PHASE);
            let mut fetched = fetch(&local);
            if let Err(e) = &fetched {
                if retry_on_other_copy(&local, e) {
                    // the clone moved or broke since the scan: once more, on a fresh pick
                    forget_clone_in(clone_cache(), &local.path);
                    local = prepare(&s.clones_for(git, root, &url))?;
                    if !s.current(gen) {
                        return Err(CANCELLED.into());
                    }
                    phase(FETCH_PHASE);
                    fetched = fetch(&local);
                }
            }
            let refs =
                fetched.map_err(|e| format!("Could not fetch the pull request's branches: {e}"))?;
            (local, refs)
        }
    };
    if !s.current(gen) {
        return Err(CANCELLED.into());
    }
    phase(files_phase(local.kind));
    let patch_auth = match s.source {
        Source::Remote(_) => deps.auth.as_ref(),
        Source::Local(_) => None,
    };
    // numstat (it diffs every file) runs beside the list (`--raw`, which does not), so the
    // list is on screen after the cheap command and the counts follow
    let counts = spawn_counts(s, gen, git, &local, &refs, &deps);
    let mut files = file_list(git, &local, &refs, patch_auth)
        .map_err(|e| format!("Could not list the changed files: {e}"))?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let index = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.clone(), i))
        .collect();
    let sess = Session {
        repo: repo.clone(),
        pr_id: pr.id,
        local,
        refs,
        files,
        index,
        deps,
    };
    Ok((sess, counts))
}

type CountsResult = Result<Vec<FileCounts>, String>;

/// numstat of an open, running on its own thread (or, if no thread could start, to be run
/// by the open itself once the list is out).
enum PendingCounts {
    Running(Receiver<CountsResult>),
    Inline,
}

fn spawn_counts(
    s: &Arc<Shared>,
    gen: u64,
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    deps: &Arc<ReviewDeps>,
) -> PendingCounts {
    let (tx, rx) = channel();
    let (s2, git, local, refs, deps) = (
        s.clone(),
        git.clone(),
        local.clone(),
        refs.clone(),
        deps.clone(),
    );
    let spawned = std::thread::Builder::new()
        .name("review-numstat".into())
        .spawn(move || {
            let _ = tx.send(run_counts(&s2, gen, &git, &local, &refs, &deps));
        });
    match spawned {
        Ok(_) => PendingCounts::Running(rx),
        Err(_) => PendingCounts::Inline,
    }
}

/// `git diff --numstat` of the PR; killed as soon as the open is superseded or closed.
fn run_counts(
    s: &Shared,
    gen: u64,
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    deps: &ReviewDeps,
) -> CountsResult {
    let run = || {
        #[cfg(test)]
        {
            let hook = lock_ok(&s.counts_hook).clone();
            if let Some(h) = hook {
                h("numstat");
            }
        }
        let auth = match s.source {
            Source::Remote(_) => deps.auth.as_ref(),
            Source::Local(_) => None,
        };
        let cancel = || !s.current(gen);
        file_counts(git, local, refs, auth, &cancel).map_err(|e| match e {
            GitError::Cancelled => CANCELLED.to_string(),
            e => format!("Could not count the changed lines: {e}"),
        })
    };
    std::panic::catch_unwind(AssertUnwindSafe(run))
        .unwrap_or_else(|_| Err("Could not count the changed lines (internal error)".into()))
}

/// Waits for an open's numstat, then fills in the counts of the open session (a copy of it
/// replaces it), emits `Counts` and queues the whole-PR prefetch if the PR is small enough:
/// that decision needs the counts. Nothing happens if the open was superseded meanwhile.
fn finish_counts(s: &Arc<Shared>, gen: u64, sess: &Arc<Session>, pending: PendingCounts) {
    let result = match pending {
        PendingCounts::Running(rx) => rx
            .recv()
            .unwrap_or_else(|_| Err("Could not count the changed lines".into())),
        PendingCounts::Inline => {
            let Ok(git) = s.git() else {
                return;
            };
            run_counts(s, gen, git, &sess.local, &sess.refs, &sess.deps)
        }
    };
    let mut st = s.lock();
    if st.gen != gen {
        return;
    }
    match result {
        Ok(counts) => {
            let mut next = Session::clone(sess);
            apply_counts(&mut next.files, &counts);
            let next = Arc::new(next);
            st.session = Some(next.clone());
            s.send(
                gen,
                ReviewEvent::Counts {
                    counts: Arc::new(counts),
                },
            );
            let limit = s.whole_pr_lines.load(Ordering::Relaxed);
            if s.threads >= 2 && whole_pr_eligible(&next.files, limit) {
                st.batches.push_back(Batch {
                    paths: next.files.iter().map(|f| f.path.clone()).collect(),
                    whole_pr: true,
                });
            }
        }
        Err(e) if e == CANCELLED => return,
        Err(e) => s.send(gen, ReviewEvent::CountsFailed(e)),
    }
    drop(st);
    s.work.notify_all();
    s.ctx.request_repaint();
}

fn probe_moved(sess: &Session) -> bool {
    (sess.deps.probe)(&sess.repo, sess.pr_id)
        .is_ok_and(|remote| !same_commit_or_unknown(&remote, &sess.refs.head))
}

#[allow(dead_code)] // only `recheck` calls it, see there
fn check_stale(s: &Shared, gen: u64, sess: &Session) {
    if probe_moved(sess) {
        s.emit(gen, ReviewEvent::Stale);
    }
}

// ---- comments (fetched on their own thread; same generation rules as patches) ----

/// Fetches the PR's comments in the background and emits `Comments` or `CommentsFailed`
/// if `gen` is still current and no newer fetch was started meanwhile.
fn spawn_comment_fetch(s: &Arc<Shared>, gen: u64, sess: Arc<Session>) {
    let seq = s.comments_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let s2 = s.clone();
    let spawned = std::thread::Builder::new()
        .name("review-comments".into())
        .spawn(move || comment_job(&s2, gen, seq, &sess));
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::CommentsFailed("Could not start a background thread".into()),
        );
    }
}

fn comment_job(s: &Shared, gen: u64, seq: u64, sess: &Session) {
    let fetched = std::panic::catch_unwind(AssertUnwindSafe(|| {
        (sess.deps.comments)(&sess.repo, sess.pr_id)
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    let ev = match fetched {
        Ok(list) => ReviewEvent::Comments {
            skipped: list.skipped.len(),
            threads: build_threads(list.comments),
        },
        Err(e) => ReviewEvent::CommentsFailed(format!("Could not load comments ({e})")),
    };
    let st = s.lock();
    if st.gen != gen || s.comments_seq.load(Ordering::SeqCst) != seq {
        return;
    }
    s.send(gen, ev);
    drop(st);
    s.ctx.request_repaint();
}

// ---- end comments ----

fn open_job(s: &Arc<Shared>, gen: u64, repo: RepoRef, pr: PullRequest) {
    // one load at a time per service, then per repository in the process; a load
    // superseded while waiting does no git work
    let _serial = lock_ok(&s.load_lock);
    #[cfg(test)]
    s.load_waiters.fetch_add(1, Ordering::SeqCst);
    let repo_lock = repo_load_lock(&s.load_key(&repo));
    let _repo_serial = lock_ok(&repo_lock);
    if !s.current(gen) {
        return;
    }
    // credentials are read here, never on the UI thread
    let deps = std::panic::catch_unwind(AssertUnwindSafe(|| (s.deps)(&repo)));
    let Ok(deps) = deps else {
        return s.emit(
            gen,
            ReviewEvent::Failed("Could not read the sign-in".into()),
        );
    };
    match load(s, gen, &repo, &pr, deps) {
        Ok((sess, counts)) => {
            let sess = Arc::new(sess);
            {
                let mut st = s.lock();
                if st.gen != gen {
                    return;
                }
                st.session = Some(sess.clone());
                // sent under the lock so no Patch of this session can precede it
                s.send(
                    gen,
                    ReviewEvent::Opened {
                        refs: sess.refs.clone(),
                        files: sess.files.clone(),
                    },
                );
            }
            s.work.notify_all();
            s.ctx.request_repaint();
            spawn_comment_fetch(s, gen, sess.clone());
            // the inbox copy (no network) or the server (probe) has a different head
            if !same_commit_or_unknown(&pr.source_commit, &sess.refs.head) || probe_moved(&sess) {
                s.emit(gen, ReviewEvent::Stale);
            }
            // numstat has been running meanwhile; still under the load locks, so a reopen
            // of this repository waits for it (or, superseding it, kills it)
            finish_counts(s, gen, &sess, counts);
        }
        Err(msg) => s.emit(gen, ReviewEvent::Failed(msg)),
    }
}

/// A built patch: rows, whether it renders plain, and its layout.
type Built = (Arc<Vec<Row>>, bool, Arc<PatchLayout>);

fn build_patch(s: &Shared, sess: &Session, path: &str) -> Result<Built, String> {
    #[cfg(test)]
    {
        let hook = lock_ok(&s.patch_hook).clone();
        if let Some(h) = hook {
            h(path);
        }
    }
    let Some(&i) = sess.index.get(path) else {
        return Err("This file is not part of the pull request".into());
    };
    let f = &sess.files[i];
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    let text = file_patch(git, &sess.local, &sess.refs, f, s.patch_auth(sess))
        .map_err(|e| format!("Could not load the diff: {e}"))?;
    Ok(built_from(&text))
}

/// Rows, plain flag and layout of one file's patch text (from `file_patch` or a batch:
/// the same bytes, so the same rows).
fn built_from(text: &str) -> Built {
    let rows = parse_patch(text);
    let plain = plain_rows(&rows);
    let layout = Arc::new(PatchLayout::compute(&rows, plain));
    (Arc::new(rows), plain, layout)
}

/// Built patches of a batch, and the paths it could not serve.
type BatchOut = (Vec<(String, Built)>, Vec<String>);

fn run_batch(s: &Shared, gen: u64, sess: &Session, batch: &Batch) -> Result<BatchOut, String> {
    #[cfg(test)]
    {
        let hook = lock_ok(&s.batch_hook).clone();
        if let Some(h) = hook {
            h(if batch.whole_pr { "whole" } else { "near" });
        }
    }
    let files: Vec<&DiffFile> = batch
        .paths
        .iter()
        .filter_map(|p| sess.index.get(p).map(|&i| &sess.files[i]))
        .collect();
    let git = s.git()?;
    // closing or reopening kills the running `git diff` (and its children) within ~50 ms
    let cancelled = || !s.current(gen);
    let out = batch_patches_cancellable(
        git,
        &sess.local,
        &sess.refs,
        &files,
        s.patch_auth(sess),
        &cancelled,
    )
    .map_err(|e| e.to_string())?;
    let built = out
        .found
        .into_iter()
        .map(|(i, text)| (files[i].path.clone(), built_from(&text)))
        .collect();
    let missing = out
        .missing
        .into_iter()
        .map(|i| files[i].path.clone())
        .collect();
    Ok((built, missing))
}

/// Runs one batch and puts its patches into the cache (never replacing a cached one). A
/// queued request for one of its files is answered from the batch. Whatever a neighbour
/// batch could not serve (or all of it, on an error or a panic) is prefetched per file.
fn batch_job(s: &Shared, gen: u64, sess: &Session, batch: Batch) {
    let result = if batch.paths.is_empty() {
        Ok((Vec::new(), Vec::new()))
    } else {
        std::panic::catch_unwind(AssertUnwindSafe(|| run_batch(s, gen, sess, &batch)))
            .unwrap_or_else(|_| Err("internal error".into()))
    };
    s.batches_run.fetch_add(1, Ordering::Relaxed);
    let mut st = s.lock();
    if st.gen != gen {
        s.batches_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if !batch.whole_pr {
        for p in &batch.paths {
            st.batching.remove(p);
        }
    }
    let (built, missing) = match result {
        Ok(r) => r,
        Err(_) => (Vec::new(), batch.paths.clone()),
    };
    let mut served = false;
    for (path, (rows, plain, layout)) in built {
        if st.cache.contains(&path) {
            continue;
        }
        st.cache.insert(path.clone(), rows.clone(), layout.clone());
        // asked for meanwhile but not started yet: answer now instead of building it again
        if let Some(pos) = st.queue.iter().position(|j| j.path == path && j.wanted) {
            st.queue.remove(pos);
            st.queued.remove(&path);
            serve(s, &mut st, gen, sess, &path, (rows, plain, layout));
            served = true;
        } else {
            st.prefetched(&path, &rows, plain);
        }
    }
    if !batch.whole_pr {
        for p in &missing {
            st.prefetch_one(p);
        }
    }
    drop(st);
    s.work.notify_all();
    if served {
        s.ctx.request_repaint();
    }
}

/// Under the state lock: sends a built patch to the screen, asks for its colours (for the
/// screen if it is the file on screen, else in the background) and prefetches its
/// neighbours.
fn serve(s: &Shared, st: &mut State, gen: u64, sess: &Session, path: &str, built: Built) {
    let (rows, plain, layout) = built;
    if st.focus.is_none() {
        st.focus = Some(path.to_string());
    }
    s.send(
        gen,
        ReviewEvent::Patch {
            path: path.to_string(),
            rows: rows.clone(),
            plain,
            layout,
        },
    );
    if st.focus.as_deref() == Some(path) {
        request_highlight(s, st, gen, path, &rows, plain);
    } else {
        // asked for, then passed (j/k held): not on screen any more
        st.prefetched(path, &rows, plain);
    }
    st.prefetch_around(sess, path);
}

// --- syntax highlighting: runs after a patch was served, never for `plain` files ---

/// Called under the state lock right after a `Patch` event: sends the remembered colours of
/// a recently highlighted file at once, otherwise queues a highlight job for the screen
/// (or tells the running one to send its colours again).
fn request_highlight(
    s: &Shared,
    st: &mut State,
    gen: u64,
    path: &str,
    rows: &Arc<Vec<Row>>,
    plain: bool,
) {
    if plain {
        return;
    }
    if let Some(spans) = st.hl.request(path, rows) {
        s.send(gen, highlighted(path, 0..rows.len(), rows.len(), spans));
    }
}

fn highlighted(path: &str, range: Range<usize>, rows: usize, spans: Arc<Spans>) -> ReviewEvent {
    ReviewEvent::Highlighted {
        path: path.to_string(),
        rows,
        range,
        spans,
    }
}

/// Colours one file hunk by hunk from the rows on screen, handing out each step while the
/// file is on screen. Abandoned (between two rows) when the PR is closed or reopened, and
/// (between two steps) when the file is no longer on screen or near it.
fn highlight_job(s: &Shared, gen: u64, gen_now: &AtomicU64, job: HlJob) {
    let n = job.rows.len();
    // a panic while highlighting leaves that file plain and the worker alive
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        {
            let hook = lock_ok(&s.hl_hook).clone();
            if let Some(h) = hook {
                h(&job.path);
            }
        }
        let cancelled = || gen_now.load(Ordering::SeqCst) != gen;
        let on_screen = || s.visible_row(&job.path);
        let mut emit = |range: Range<usize>, spans: &[RowSpans]| {
            let st = s.lock();
            if st.gen != gen || !st.near_focus(&job.path) {
                return Next::Stop;
            }
            if st.focus.as_deref() != Some(job.path.as_str()) {
                // a prefetched file: its colours are remembered, sent once it is shown
                return Next::Continue;
            }
            s.send(
                gen,
                highlighted(&job.path, range, n, Arc::new(spans.to_vec())),
            );
            drop(st);
            s.ctx.request_repaint();
            if job.resend.swap(false, Ordering::SeqCst) {
                Next::Resend
            } else {
                Next::Continue
            }
        };
        let budget = Duration::from_millis(s.hl_budget_ms.load(Ordering::Relaxed));
        highlight_chunked(
            &job.path,
            &job.rows,
            Chunking {
                rows: CHUNK_ROWS,
                budget,
                cancelled: &cancelled,
                on_screen: &on_screen,
                emit: &mut emit,
            },
        )
    }));
    s.hl_ended.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    lock_ok(&s.hl_done).push((job.path.clone(), Instant::now()));
    let mut st = s.lock();
    if st.gen != gen {
        return;
    }
    let spans = match result {
        Ok((spans, HlEnd::Done | HlEnd::OverBudget)) => Some(Arc::new(spans)),
        Ok((_, HlEnd::Cancelled)) | Err(_) => None,
    };
    let resend = st.hl.finish(&job.path, spans.clone());
    let on_screen = st.focus.as_deref() == Some(job.path.as_str());
    if let (true, true, Some(spans)) = (resend, on_screen, spans) {
        // shown again after its last step: everything at once
        s.send(gen, highlighted(&job.path, 0..n, n, spans));
        drop(st);
        s.ctx.request_repaint();
    }
}

// --- end syntax highlighting ---

/// Clears a job's `in_flight` entry if the worker unwinds before handling it, so the path
/// can be requested again.
struct InFlightGuard<'a> {
    s: &'a Shared,
    gen: u64,
    path: Option<String>,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let mut st = self.s.lock();
            if st.gen == self.gen {
                st.in_flight.remove(&path);
            }
        }
    }
}

/// Batches that may run at once: every worker but the reserved one (at least one).
fn batch_slots(threads: usize) -> usize {
    threads.saturating_sub(1).max(1)
}

/// Frees a batch slot when the batch ends, however it ends.
struct BatchSlot<'a>(&'a Shared);

impl Drop for BatchSlot<'_> {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.batches_running = st.batches_running.saturating_sub(1);
        drop(st);
        self.0.work.notify_all();
    }
}

/// Keeps `workers_live` accurate however the worker exits.
struct LiveWorker<'a>(&'a Shared);

impl Drop for LiveWorker<'_> {
    fn drop(&mut self) {
        self.0.workers_live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `reserved`: the worker kept for patches (with two or more workers, the first): it takes
/// neither highlight nor batch jobs, so the file on screen is never stuck behind either.
fn worker(s: &Shared, reserved: bool) {
    s.workers_live.fetch_add(1, Ordering::SeqCst);
    let _live = LiveWorker(s);
    loop {
        let task = {
            let mut st = s.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(t) = st.take_task(reserved, batch_slots(s.threads)) {
                    break t;
                }
                st = s.work.wait(st).unwrap_or_else(|p| p.into_inner());
            }
        };
        match task {
            Task::Patch { gen, sess, path } => patch_job(s, gen, &sess, path),
            Task::Highlight { gen, gen_now, job } => highlight_job(s, gen, &gen_now, job),
            Task::Batch { gen, sess, batch } => {
                let _slot = BatchSlot(s);
                batch_job(s, gen, &sess, batch);
            }
        }
    }
}

/// Builds one file's patch (marked in flight by `take_task`) and caches it; a file the
/// screen asked for is served, a prefetched one may get its colours in the background.
fn patch_job(s: &Shared, gen: u64, sess: &Session, path: String) {
    let mut guard = InFlightGuard {
        s,
        gen,
        path: Some(path.clone()),
    };
    // a panic in git output handling or parsing fails this file only
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| build_patch(s, sess, &path)))
        .unwrap_or_else(|_| Err("Internal error while preparing this file's diff".into()));
    s.built.fetch_add(1, Ordering::Relaxed);
    let mut st = s.lock();
    guard.path = None; // handled below, under this lock
    if st.gen != gen {
        return;
    }
    let wanted = st.in_flight.remove(&path).unwrap_or(false);
    match result {
        Ok((rows, plain, layout)) => {
            st.cache.insert(path.clone(), rows.clone(), layout.clone());
            if wanted {
                serve(s, &mut st, gen, sess, &path, (rows, plain, layout));
            } else {
                st.prefetched(&path, &rows, plain);
            }
        }
        Err(error) if wanted => s.send(gen, ReviewEvent::PatchFailed { path, error }),
        Err(_) => {}
    }
    drop(st);
    s.work.notify_all();
    if wanted {
        s.ctx.request_repaint();
    }
}

pub struct ReviewService {
    shared: Arc<Shared>,
    rx: Mutex<Receiver<(u64, ReviewEvent)>>,
    /// The generation whose `recheck` is out: further ones for it are dropped until it is
    /// back (one in flight per open PR). A probe still out for an older PR blocks nothing.
    rechecking: Arc<Mutex<Option<u64>>>,
}

/// Clears the recheck in-flight mark of its generation when the recheck ends (also on a
/// panic); a newer generation's mark is left alone.
struct RecheckDone(Arc<Mutex<Option<u64>>>, u64);

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

    /// Demo/test path: uses `repo_path` (holding both branches) directly, no fetch.
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

    fn build(
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
            .spawn(super::highlight::warm);
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
    fn set_patch_hook(&self, hook: PatchHook) {
        *self.shared.patch_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_highlight_hook(&self, hook: PatchHook) {
        *self.shared.hl_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_load_hook(&self, hook: PatchHook) {
        *self.shared.load_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_batch_hook(&self, hook: PatchHook) {
        *self.shared.batch_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_counts_hook(&self, hook: PatchHook) {
        *self.shared.counts_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_locator(&self, locator: Locator) {
        *self.shared.locator.lock().unwrap() = Some(locator);
    }

    /// Runs git through `git` (its own spawn counter) instead of the shared one.
    #[cfg(test)]
    fn use_git(&self, git: Git) {
        assert!(self.shared.git.set(git).is_ok(), "git already in use");
    }

    #[cfg(test)]
    fn set_whole_pr_lines(&self, lines: u64) {
        self.shared.whole_pr_lines.store(lines, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn set_cache_cap(&self, bytes: usize) {
        self.shared.lock().cache = PatchCache::new(bytes);
    }

    #[cfg(test)]
    fn cache_bytes(&self) -> usize {
        self.shared.lock().cache.bytes()
    }

    #[cfg(test)]
    fn batches_run(&self) -> u64 {
        self.shared.batches_run.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn batches_dropped(&self) -> u64 {
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

#[cfg(test)]
mod tests;
