//! Whether the PR merges cleanly into its destination, and the merged text of a conflicted
//! file: local git, each on a thread of its own.

use super::*;

/// Trial-merges the open PR into its destination and emits `Conflicts`.
pub(super) fn spawn_conflict_check(s: &Arc<Shared>, gen: u64, sess: Arc<Session>) {
    let s2 = s.clone();
    let spawned = std::thread::Builder::new()
        .name("review-conflicts".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| check(&s2, &sess)))
                .unwrap_or_else(|_| Err("internal error".into()));
            s2.emit(gen, ReviewEvent::Conflicts(result));
        });
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::Conflicts(Err("Could not start a background thread".into())),
        );
    }
}

/// Reads `path` of the merged tree `tree` (conflict markers in it) and emits `ConflictText`.
pub(super) fn spawn_conflict_text(
    s: &Arc<Shared>,
    gen: u64,
    sess: Arc<Session>,
    tree: String,
    path: String,
) {
    let s2 = s.clone();
    let p2 = path.clone();
    let spawned = std::thread::Builder::new()
        .name("review-conflict-text".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let git = s2.git().map_err(|e| format!("Git is unavailable: {e}"))?;
                file_text(git, &sess.local, &tree, &p2, range_auth(&s2, &sess))
                    .map(Arc::new)
                    .map_err(|e| format!("Could not read the merged file: {e}"))
            }))
            .unwrap_or_else(|_| Err("internal error".into()));
            s2.emit(gen, ReviewEvent::ConflictText { path: p2, result });
        });
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::ConflictText {
                path,
                result: Err("Could not start a background thread".into()),
            },
        );
    }
}

fn check(s: &Shared, sess: &Session) -> Result<ConflictReport, String> {
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    merge_check(git, &sess.local, &sess.refs, range_auth(s, sess))
        .map_err(|e| format!("Could not test the merge: {e}"))
}
