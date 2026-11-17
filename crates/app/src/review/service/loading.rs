use super::*;

/// Counts concurrent loads for the lifetime of the value.
pub(super) struct LoadCount<'a>(&'a Shared);

impl<'a> LoadCount<'a> {
    pub(super) fn start(s: &'a Shared) -> LoadCount<'a> {
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

pub(super) fn load(
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

pub(super) type CountsResult = Result<Vec<FileCounts>, String>;

/// numstat of an open, running on its own thread (or, if no thread could start, to be run
/// by the open itself once the list is out).
pub(super) enum PendingCounts {
    Running(Receiver<CountsResult>),
    Inline,
}

pub(super) fn spawn_counts(
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
pub(super) fn run_counts(
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
pub(super) fn finish_counts(
    s: &Arc<Shared>,
    gen: u64,
    sess: &Arc<Session>,
    pending: PendingCounts,
) {
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

pub(super) fn probe_moved(sess: &Session) -> bool {
    (sess.deps.probe)(&sess.repo, sess.pr_id)
        .is_ok_and(|remote| !same_commit_or_unknown(&remote, &sess.refs.head))
}

#[allow(dead_code)] // only `recheck` calls it, see there
pub(super) fn check_stale(s: &Shared, gen: u64, sess: &Session) {
    if probe_moved(sess) {
        s.emit(gen, ReviewEvent::Stale);
    }
}

// ---- comments (fetched on their own thread; same generation rules as patches) ----

/// Fetches the PR's comments in the background and emits `Comments` or `CommentsFailed`
/// if `gen` is still current and no newer fetch was started meanwhile.
pub(super) fn spawn_comment_fetch(s: &Arc<Shared>, gen: u64, sess: Arc<Session>) {
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

pub(super) fn comment_job(s: &Shared, gen: u64, seq: u64, sess: &Session) {
    let fetched = std::panic::catch_unwind(AssertUnwindSafe(|| {
        (sess.deps.comments)(&sess.repo, sess.pr_id)
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    let evs = match fetched {
        Ok(list) => vec![
            ReviewEvent::Tasks(list.tasks),
            ReviewEvent::Comments {
                skipped: list.skipped.len(),
                threads: build_threads(list.comments),
            },
        ],
        Err(e) => vec![ReviewEvent::CommentsFailed(format!(
            "Could not load comments ({e})"
        ))],
    };
    let st = s.lock();
    if st.gen != gen || s.comments_seq.load(Ordering::SeqCst) != seq {
        return;
    }
    for ev in evs {
        s.send(gen, ev);
    }
    drop(st);
    s.ctx.request_repaint();
}

// ---- end comments ----

pub(super) fn open_job(s: &Arc<Shared>, gen: u64, repo: RepoRef, pr: PullRequest) {
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
            spawn_commit_info(s, gen, sess.clone());
            spawn_conflict_check(s, gen, sess.clone());
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
