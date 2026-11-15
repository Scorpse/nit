//! A background thread for writes that the UI thread must not wait for (viewed marks, the
//! file view, settings): jobs run one at a time in the order they were queued, so two writes
//! of one key never swap. Dropping the writer runs everything still queued first.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::db::{Store, StoreResult};

type Job = Box<dyn FnOnce(&Store) + Send>;

/// Decrements the pending count when a job ends, however it ends, and only then tells a
/// waiting `flush`, so `pending()` is already 0 when `flush` returns.
struct PendingDone(Arc<AtomicUsize>, Option<Sender<()>>);

impl Drop for PendingDone {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        if let Some(done) = self.1.take() {
            let _ = done.send(());
        }
    }
}

pub struct StoreWriter {
    store: Arc<Mutex<Store>>,
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    failures: Receiver<String>,
    failure_tx: Sender<String>,
    pending: Arc<AtomicUsize>,
}

impl StoreWriter {
    pub fn new(store: Arc<Mutex<Store>>) -> StoreWriter {
        let (tx, rx) = channel::<Job>();
        let (failure_tx, failures) = channel();
        let worker_store = store.clone();
        let thread = std::thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || {
                for job in rx {
                    let guard = worker_store.lock().unwrap_or_else(|p| p.into_inner());
                    // a panicking job must not end the thread: later jobs (and `flush`) need it
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| job(&guard)));
                }
            })
            .ok();
        StoreWriter {
            store,
            // without a thread, jobs run on the caller (still correct, only slower)
            tx: thread.as_ref().map(|_| tx),
            thread,
            failures,
            failure_tx,
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Queues `f`; when it fails, `failure` is what `take_failure` returns (a short UI text,
    /// never the error's detail).
    pub fn write(
        &self,
        failure: impl Into<String>,
        f: impl FnOnce(&Store) -> StoreResult<()> + Send + 'static,
    ) {
        let failure = failure.into();
        let report = self.failure_tx.clone();
        self.submit(move |s| {
            if f(s).is_err() {
                let _ = report.send(failure);
            }
        });
    }

    /// Queues `f`, which reports its own outcome.
    pub fn submit(&self, f: impl FnOnce(&Store) + Send + 'static) {
        self.submit_then(f, None);
    }

    fn submit_then(&self, f: impl FnOnce(&Store) + Send + 'static, done: Option<Sender<()>>) {
        let pending = self.pending.clone();
        pending.fetch_add(1, Ordering::SeqCst);
        let job: Job = Box::new(move |s| {
            // decremented however the job ends, also when it panics
            let _done = PendingDone(pending, done);
            f(s);
        });
        let job = match &self.tx {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(e) => e.0,
            },
            None => job,
        };
        let guard = self.store.lock().unwrap_or_else(|p| p.into_inner());
        job(&guard);
    }

    /// The last failure reported since the previous call.
    pub fn take_failure(&self) -> Option<String> {
        self.failures.try_iter().last()
    }

    /// Jobs queued or running.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Waits until every job queued so far has run.
    pub fn flush(&self) {
        let (done_tx, done_rx) = channel();
        self.submit_then(|_| {}, Some(done_tx));
        let _ = done_rx.recv();
    }
}

impl Drop for StoreWriter {
    /// Runs what is still queued, then stops the thread.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prcore::model::RepoRef;
    use std::time::{Duration, Instant};

    fn repo() -> RepoRef {
        RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: "acme".into(),
            slug: "api".into(),
        }
    }

    #[test]
    fn writes_run_in_order_off_the_caller_and_flush_on_drop() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let w = StoreWriter::new(store.clone());
        // the caller does not wait for the store while the writer is blocked on it
        let guard = store.lock().unwrap();
        let t = Instant::now();
        for i in 0..50 {
            let viewed = i % 2 == 0;
            w.write("Could not save", move |s| {
                s.set_viewed(&repo(), 1, "a.rs", &format!("o{i}"), viewed)
            });
        }
        w.write("Could not save", |s| {
            s.set_viewed(&repo(), 1, "a.rs", "last", true)
        });
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(w.pending() > 0);
        drop(guard);
        drop(w);
        let m = store.lock().unwrap().viewed_map(&repo(), 1).unwrap();
        assert_eq!(m["a.rs"], "last", "the last write of a key wins");
    }

    #[test]
    fn failure_is_reported_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let w = StoreWriter::new(store);
        w.write("Could not save the viewed mark", |_| {
            Err(crate::db::StoreError::Corrupt)
        });
        w.write("fine", |_| Ok(()));
        w.flush();
        assert_eq!(w.pending(), 0);
        assert_eq!(
            w.take_failure().as_deref(),
            Some("Could not save the viewed mark")
        );
        assert_eq!(w.take_failure(), None);
    }

    #[test]
    fn a_panicking_job_does_not_leave_pending_set() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let w = StoreWriter::new(store.clone());
        w.submit(|_| panic!("injected writer panic"));
        let t = Instant::now();
        while w.pending() != 0 {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "pending stuck at {}",
                w.pending()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // later writes still run (on the writer thread or, if it is gone, on the caller)
        w.write("Could not save", |s| {
            s.set_viewed(&repo(), 1, "a.rs", "o", true)
        });
        w.flush();
        assert_eq!(w.pending(), 0);
        assert_eq!(w.take_failure(), None);
        let m = store.lock().unwrap().viewed_map(&repo(), 1).unwrap();
        assert_eq!(m["a.rs"], "o");
    }
}
