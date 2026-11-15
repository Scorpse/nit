use super::*;

pub(super) enum Source {
    /// Real path: pick a clone or mirror, fetch the PR over HTTPS.
    Remote(UrlFor),
    /// Demo/test path: a repository that already has both branches.
    Local(PathBuf),
}

/// Replaced by a copy with the counts filled in when numstat arrives (`files` changes, the
/// rest stays).
#[derive(Clone)]
pub(super) struct Session {
    pub(super) repo: RepoRef,
    pub(super) pr_id: u64,
    pub(super) local: LocalRepo,
    pub(super) refs: PrRefs,
    pub(super) files: Vec<DiffFile>,
    pub(super) index: HashMap<String, usize>,
    pub(super) deps: Arc<ReviewDeps>,
}

/// Speculative patches for several files from one `git diff`; fills the cache only.
pub(super) struct Batch {
    pub(super) paths: Vec<String>,
    /// The whole PR (lowest priority; its paths are not reserved in `batching`).
    pub(super) whole_pr: bool,
}

pub(super) struct Job {
    pub(super) path: String,
    /// Requested by the UI (emit an event); false for prefetch (cache only).
    pub(super) wanted: bool,
    /// Queued as the selected file (front of the queue); only the newest few are kept.
    pub(super) priority: bool,
}

pub(super) struct State {
    pub(super) gen: u64,
    pub(super) session: Option<Arc<Session>>,
    pub(super) queue: VecDeque<Job>,
    pub(super) queued: HashSet<String>,
    /// path -> wanted
    pub(super) in_flight: HashMap<String, bool>,
    pub(super) cache: PatchCache,
    /// Highlight jobs; workers take them only when no patch is waiting.
    pub(super) hl: HighlightQueue,
    /// Batch jobs, taken only when no patch (and, by highlighting workers, no highlight) waits.
    pub(super) batches: VecDeque<Batch>,
    /// Paths of queued or running neighbour batches (not asked for again meanwhile).
    pub(super) batching: HashSet<String>,
    /// Batches running now, of any generation (at most `batch_slots`).
    pub(super) batches_running: usize,
    /// The file last served to the screen: highlights of files outside its prefetch window
    /// are abandoned.
    pub(super) focus: Option<String>,
    /// `gen`, readable without the lock: a running highlight checks it before every row.
    pub(super) gen_now: Arc<AtomicU64>,
    pub(super) shutdown: bool,
}

/// The next piece of work of a worker.
pub(super) enum Task {
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
    pub(super) fn new() -> State {
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

    pub(super) fn reset(&mut self) {
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
    pub(super) fn busy(&self, path: &str) -> bool {
        self.cache.contains(path)
            || self.queued.contains(path)
            || self.in_flight.contains_key(path)
            || self.batching.contains(path)
    }

    /// Queues a cache-only per-file job (the fallback when a batch cannot serve a file).
    pub(super) fn prefetch_one(&mut self, path: &str) {
        if !self.busy(path) {
            self.queued.insert(path.to_string());
            self.queue.push_back(Job {
                path: path.to_string(),
                wanted: false,
                priority: false,
            });
        }
    }

    pub(super) fn enqueue(&mut self, path: String, wanted: bool, priority: bool) {
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
    pub(super) fn drop_old_priority(&mut self) {
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
    pub(super) fn prefetch_around(&mut self, sess: &Session, path: &str) {
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
    pub(super) fn push_neighbour_batch(&mut self, paths: Vec<String>) {
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
    pub(super) fn pop_batch(&mut self) -> Option<Batch> {
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
    pub(super) fn patch_task(&mut self, pos: usize, sess: Arc<Session>) -> Option<Task> {
        let job = self.queue.remove(pos)?;
        self.queued.remove(&job.path);
        self.in_flight.insert(job.path.clone(), job.wanted);
        Some(Task::Patch {
            gen: self.gen,
            sess,
            path: job.path,
        })
    }

    pub(super) fn highlight_task(&self, job: HlJob) -> Task {
        Task::Highlight {
            gen: self.gen,
            gen_now: self.gen_now.clone(),
            job,
        }
    }

    /// The next job of a worker, most wanted first: a patch the screen asked for, the
    /// colours of the file on screen, a prefetch patch, the colours of a prefetched file,
    /// a batch (at most `batch_slots` at once). The reserved worker takes patches only.
    pub(super) fn take_task(&mut self, reserved: bool, batch_slots: usize) -> Option<Task> {
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
    pub(super) fn near_focus(&self, path: &str) -> bool {
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
    pub(super) fn prefetched(&mut self, path: &str, rows: &Arc<Vec<Row>>, plain: bool) {
        if !plain && self.near_focus(path) {
            self.hl.request_background(path, rows);
        }
    }
}

/// Whether `files` is small enough to prefetch whole: at most `limit` changed lines in
/// total and no plain file.
pub(super) fn whole_pr_eligible(files: &[DiffFile], limit: u64) -> bool {
    let total: u64 = files
        .iter()
        .map(|f| u64::from(f.additions) + u64::from(f.deletions))
        .sum();
    !files.is_empty()
        && files.iter().all(|f| f.counts_known)
        && total <= limit
        && !files.iter().any(is_plain)
}
