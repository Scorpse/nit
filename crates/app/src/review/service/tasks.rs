//! Adding a task to the open PR, on a thread of its own.

use super::*;

/// Creates a task with `content` and emits `TaskCreated`, on its own thread.
pub(super) fn spawn_create_task(s: &Arc<Shared>, gen: u64, sess: Arc<Session>, content: String) {
    let s2 = s.clone();
    let spawned = std::thread::Builder::new()
        .name("review-task".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                (sess.deps.create_task)(&sess.repo, sess.pr_id, &content)
            }))
            .unwrap_or_else(|_| Err("internal error".into()));
            s2.emit(gen, ReviewEvent::TaskCreated(result));
        });
    if spawned.is_err() {
        s.emit(
            gen,
            ReviewEvent::TaskCreated(Err("Could not start a background thread".into())),
        );
    }
}
