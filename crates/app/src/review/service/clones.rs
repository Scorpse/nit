use super::*;

/// Shortest time between two automatic rescans of `clone_root` (when no clone matched).
pub(super) const CLONE_RESCAN_EVERY: Duration = Duration::from_secs(10);

/// One scan of `clone_root`.
pub(super) struct CloneScan {
    pub(super) root: PathBuf,
    pub(super) found: Vec<LocalClone>,
    pub(super) at: Instant,
}

/// Last `clone_root` scan in this process. Shared by every review and replaced by a rescan
/// in Settings; a different root scans again.
pub(super) fn clone_cache() -> &'static Mutex<Option<CloneScan>> {
    static CACHE: Mutex<Option<CloneScan>> = Mutex::new(None);
    &CACHE
}

/// Records a fresh scan of `root` (Settings' rescan), so the next review uses it.
pub fn remember_clones(root: &Path, found: &[LocalClone]) {
    remember_in(clone_cache(), root, found);
}

pub(super) fn remember_in(cache: &Mutex<Option<CloneScan>>, root: &Path, found: &[LocalClone]) {
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
pub(super) fn clones_in(
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
pub(super) fn forget_clone_in(cache: &Mutex<Option<CloneScan>>, path: &Path) {
    if let Some(scan) = lock_ok(cache).as_mut() {
        scan.found.retain(|c| c.path != path);
    }
}

/// Whether a failed fetch into `local` should be retried once on another local copy: the
/// copy is a discovered clone that git could not even run in (an i/o error, or the directory
/// is gone). A mirror, or git failing inside the clone (auth, network), is not retried.
pub(super) fn retry_on_other_copy(local: &LocalRepo, err: &GitError) -> bool {
    local.kind == RepoKind::Clone
        && (matches!(err, GitError::Io(_) | GitError::NotInstalled) || !local.path.is_dir())
}

/// Reads both branch tips from a repository that has them and computes the merge-base.
/// What to fetch for `pr` (spec 7.1): Bitbucket fetches both branches by name; GitHub and
/// GitLab fetch the forge's own PR ref (it survives a deleted or force-pushed source branch
/// and covers fork PRs) and the destination branch.
pub(super) fn fetch_spec(pr: &PullRequest) -> FetchSpec {
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
pub(super) fn fetch_with_fallback<T, E>(
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

pub(super) fn local_refs(
    git: &Git,
    local: &LocalRepo,
    pr: &PullRequest,
) -> Result<PrRefs, GitError> {
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

pub(super) const CANCELLED: &str = "cancelled";

/// One load at a time per local copy in the whole process, like `gitlocal`'s mirror lock:
/// opening, going back and reopening (or a second review service) never run two
/// `fetch_pr` / `file_index` on the same repository at once.
pub(super) fn repo_load_lock(key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut map = lock_ok(LOCKS.get_or_init(Default::default));
    // Drop locks nobody holds, so the map follows the repositories in use.
    map.retain(|_, l| Arc::strong_count(l) > 1);
    map.entry(key.to_string()).or_default().clone()
}
