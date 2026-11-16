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
