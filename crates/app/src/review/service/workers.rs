use super::*;

/// Clears a job's `in_flight` entry if the worker unwinds before handling it, so the path
/// can be requested again.
pub(super) struct InFlightGuard<'a> {
    pub(super) s: &'a Shared,
    pub(super) gen: u64,
    pub(super) path: Option<String>,
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
pub(super) fn batch_slots(threads: usize) -> usize {
    threads.saturating_sub(1).max(1)
}

/// Frees a batch slot when the batch ends, however it ends.
pub(super) struct BatchSlot<'a>(&'a Shared);

impl Drop for BatchSlot<'_> {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.batches_running = st.batches_running.saturating_sub(1);
        drop(st);
        self.0.work.notify_all();
    }
}

/// Keeps `workers_live` accurate however the worker exits.
pub(super) struct LiveWorker<'a>(&'a Shared);

impl Drop for LiveWorker<'_> {
    fn drop(&mut self) {
        self.0.workers_live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `reserved`: the worker kept for patches (with two or more workers, the first): it takes
/// neither highlight nor batch jobs, so the file on screen is never stuck behind either.
pub(super) fn worker(s: &Shared, reserved: bool) {
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
pub(super) fn patch_job(s: &Shared, gen: u64, sess: &Session, path: String) {
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
