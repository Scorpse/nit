use super::*;

/// Files with more changed lines than this render plain (no highlight, no word diff).
pub const PLAIN_CHANGED_LINES: u32 = 20_000;

/// Files prefetched on each side of a requested file, in index order.
pub const PREFETCH_RADIUS: usize = 5;

/// How deep below `clone_root` existing clones are searched for.
pub(super) const CLONE_SCAN_DEPTH: usize = 4;

/// Queued (not started) priority requests kept; older ones are dropped, so holding j/k
/// does not build a patch for every file passed on the way.
pub(super) const KEEP_PRIORITY: usize = 3;

/// Queued (not started) neighbour batches kept, newest first; older ones are dropped.
pub(super) const KEEP_BATCHES: usize = 2;

/// A PR with at most this many changed lines in total (and no plain file) is prefetched
/// whole with one `git diff` once it is open.
pub const WHOLE_PR_LINES: u64 = 20_000;

/// Progress shown before the first clone of a repository's mirror.
pub const CLONE_PHASE: &str =
    "Cloning the repository for the first time (can take several minutes for large repos)...";
/// The clone phase once git reports progress: "receiving objects 45%".
pub(super) fn clone_progress_phase(p: &GitProgress) -> String {
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

pub(super) fn files_phase(kind: RepoKind) -> &'static str {
    match kind {
        RepoKind::Mirror => MIRROR_FILES_PHASE,
        RepoKind::Clone => LIST_PHASE,
    }
}

/// Returns the PR's current source commit as the server sees it (`Provider::pr()`).
pub type HeadProbe = Arc<dyn Fn(&RepoRef, u64) -> Result<String, String> + Send + Sync>;

/// Returns every comment of the PR (`Provider::comments()`); errors are category text.
pub type CommentFetcher = Arc<dyn Fn(&RepoRef, u64) -> Result<CommentList, String> + Send + Sync>;

/// Returns my last submitted review of the PR (`Provider::my_last_review()`); `None` when I
/// have none or the forge does not say; errors are category text.
pub type LastReviewFetcher =
    Arc<dyn Fn(&RepoRef, u64) -> Result<Option<ReviewMark>, String> + Send + Sync>;
/// Adds a task with this text to the PR (`Provider::create_task()`); errors are category text.
pub type TaskCreator = Arc<dyn Fn(&RepoRef, u64, &str) -> Result<PrTask, String> + Send + Sync>;

pub(super) type UrlFor = Arc<dyn Fn(&RepoRef) -> String + Send + Sync>;

/// What one open needs from the signed-in user: git credentials, the head probe and the
/// comment fetcher. Built on the open's own thread (credential reads never block the UI).
pub struct ReviewDeps {
    pub auth: Option<GitAuth>,
    pub probe: HeadProbe,
    pub comments: CommentFetcher,
    pub last_review: LastReviewFetcher,
    pub create_task: TaskCreator,
}

/// Called once per open, on the open's thread, with the repository opened (its account's
/// credentials and provider).
pub type DepsFn = Arc<dyn Fn(&RepoRef) -> Arc<ReviewDeps> + Send + Sync>;

#[cfg(test)]
pub(super) fn fixed_deps(
    auth: Option<GitAuth>,
    probe: HeadProbe,
    comments: CommentFetcher,
) -> DepsFn {
    let deps = Arc::new(ReviewDeps {
        auth,
        probe,
        comments,
        last_review: Arc::new(|_, _| Ok(None)),
        create_task: Arc::new(|_, _, _| Err("tasks are not available here".into())),
    });
    Arc::new(move |_: &RepoRef| deps.clone())
}

/// What changed from commit `base` to commit `head` of the open PR (`<id>^` is the parent of
/// a single commit).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitRange {
    pub base: String,
    pub head: String,
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
    /// The PR's commits, oldest first (after `Opened`).
    Commits(Vec<CommitInfo>),
    /// The commits could not be listed; the diff is unaffected.
    CommitsFailed(String),
    /// My last submitted review (`None`: none, or the forge does not say).
    LastReview(Option<ReviewMark>),
    /// My last review could not be read; the commits are still shown, none marked new.
    LastReviewFailed(String),
    /// The files changed in a commit range, with their line counts.
    RangeFiles {
        range: CommitRange,
        result: Result<Vec<DiffFile>, String>,
    },
    /// One file's patch (git's unified diff) in a commit range.
    RangePatch {
        range: CommitRange,
        path: String,
        result: Result<Arc<String>, String>,
    },
    /// The PR's tasks, sent just before `Comments`; `None`: not readable (or the forge has none).
    Tasks(Option<Vec<PrTask>>),
    /// The answer to `create_task`: the task as the server stored it, or why it was not made.
    TaskCreated(Result<PrTask, String>),
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
pub(super) fn refuses_fork(pr: &PullRequest) -> bool {
    pr.is_fork() && pr.forge == Forge::BitbucketCloud
}

/// Over the plain limit by its numstat counts (false while they are unknown). Used only to
/// plan prefetching; whether a built patch renders plain is `plain_rows`.
pub(super) fn is_plain(f: &DiffFile) -> bool {
    u64::from(f.additions) + u64::from(f.deletions) > u64::from(PLAIN_CHANGED_LINES)
}

/// Over the plain limit by its own rows: the added and deleted rows are exactly what
/// numstat counts for the same diff, and they are known without waiting for numstat.
pub(super) fn plain_rows(rows: &[Row]) -> bool {
    let changed = rows
        .iter()
        .filter(|r| matches!(r.kind, RowKind::Add | RowKind::Del))
        .count();
    changed > PLAIN_CHANGED_LINES as usize
}
