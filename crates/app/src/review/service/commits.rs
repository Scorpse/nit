use super::*;

/// The commits of the open PR (local git, oldest first) and, from the forge, where my last
/// review stands. Each answers on its own thread-free path: the log is a quick local command
/// run here, the review mark is one request on a thread of its own (like the comments).
pub(super) fn spawn_commit_info(s: &Arc<Shared>, gen: u64, sess: Arc<Session>) {
    send_commits(s, gen, &sess);
    let s2 = s.clone();
    let spawned = std::thread::Builder::new()
        .name("review-mark".into())
        .spawn(move || mark_job(&s2, gen, &sess));
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::LastReviewFailed("Could not start a background thread".into()),
        );
    }
}

/// Lists the files changed in `range` on a thread of its own.
pub(super) fn spawn_range_files(s: &Arc<Shared>, gen: u64, sess: Arc<Session>, range: CommitRange) {
    let s2 = s.clone();
    let r2 = range.clone();
    let spawned = std::thread::Builder::new()
        .name("review-range-files".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(AssertUnwindSafe(|| range_files(&s2, &sess, &r2)))
                    .unwrap_or_else(|_| Err("internal error".into()));
            s2.emit(gen, ReviewEvent::RangeFiles { range: r2, result });
        });
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::RangeFiles {
                range,
                result: Err("Could not start a background thread".into()),
            },
        );
    }
}

/// Builds one file's patch in `range` on a thread of its own.
pub(super) fn spawn_range_patch(
    s: &Arc<Shared>,
    gen: u64,
    sess: Arc<Session>,
    range: CommitRange,
    file: DiffFile,
) {
    let s2 = s.clone();
    let (r2, path) = (range.clone(), file.path.clone());
    let spawned = std::thread::Builder::new()
        .name("review-range-patch".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(AssertUnwindSafe(|| range_patch(&s2, &sess, &r2, &file)))
                    .unwrap_or_else(|_| Err("internal error".into()));
            s2.emit(
                gen,
                ReviewEvent::RangePatch {
                    range: r2,
                    path: file.path,
                    result,
                },
            );
        });
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::RangePatch {
                range,
                path,
                result: Err("Could not start a background thread".into()),
            },
        );
    }
}

fn range_refs(sess: &Session, range: &CommitRange) -> PrRefs {
    PrRefs {
        base: range.base.clone(),
        head: range.head.clone(),
        dest: sess.refs.dest.clone(),
    }
}

/// Git credentials, needed only when the repository is a blobless mirror that fetches blobs.
pub(super) fn range_auth<'a>(s: &Shared, sess: &'a Session) -> Option<&'a GitAuth> {
    match s.source {
        Source::Remote(_) => sess.deps.auth.as_ref(),
        Source::Local(_) => None,
    }
}

fn range_files(s: &Shared, sess: &Session, range: &CommitRange) -> Result<Vec<DiffFile>, String> {
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    let refs = range_refs(sess, range);
    let mut files = file_index(git, &sess.local, &refs, range_auth(s, sess))
        .map_err(|e| format!("Could not list the changed files: {e}"))?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn range_patch(
    s: &Shared,
    sess: &Session,
    range: &CommitRange,
    file: &DiffFile,
) -> Result<Arc<String>, String> {
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    let refs = range_refs(sess, range);
    file_patch(git, &sess.local, &refs, file, range_auth(s, sess))
        .map(Arc::new)
        .map_err(|e| format!("Could not build the diff: {e}"))
}

fn send_commits(s: &Shared, gen: u64, sess: &Session) {
    let ev = match s.git() {
        Err(e) => ReviewEvent::CommitsFailed(format!("Git is unavailable: {e}")),
        Ok(git) => match commit_log(git, &sess.local, &sess.refs) {
            Ok(commits) => ReviewEvent::Commits(commits),
            Err(e) => ReviewEvent::CommitsFailed(format!("Could not list the commits: {e}")),
        },
    };
    s.emit(gen, ev);
}

fn mark_job(s: &Shared, gen: u64, sess: &Session) {
    let fetched = std::panic::catch_unwind(AssertUnwindSafe(|| {
        (sess.deps.last_review)(&sess.repo, sess.pr_id)
    }))
    .unwrap_or_else(|_| Err("internal error".into()));
    s.emit(
        gen,
        match fetched {
            Ok(mark) => ReviewEvent::LastReview(mark),
            Err(e) => ReviewEvent::LastReviewFailed(e),
        },
    );
}
