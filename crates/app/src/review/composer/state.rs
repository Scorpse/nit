//! The composer state: editor, toast, events, the `Composer` struct and its basic accessors.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    pub target: Target,
    pub text: String,
    /// The draft being edited; None for a new one.
    pub draft_id: Option<i64>,
    /// The comment type chosen in the composer (chips, or Tab / Shift+Tab).
    pub comment_type: Option<CommentType>,
    /// Take keyboard focus on the next frame it is drawn.
    pub focus: bool,
    /// The forge's suggestion syntax: "Suggest change" is offered on new-side lines.
    pub suggestions: Option<SuggestionSyntax>,
}

/// The resolve / unresolve toast.
#[derive(Clone, Debug)]
pub struct Toast {
    pub root_id: CommentId,
    pub resolved: bool,
    pub at: Instant,
    pub can_undo: bool,
}

#[derive(Debug)]
pub(super) enum Event {
    Posting(i64),
    /// `comment` is None when the server's answer could not be read (refresh instead).
    Posted {
        id: i64,
        comment: Option<Box<Comment>>,
        removed: bool,
        /// The posted tombstone was written (else: only this process knows it was posted).
        recorded: bool,
    },
    /// Claimed by another batch meanwhile: skipped here.
    Elsewhere(i64),
    Blocked {
        id: i64,
        block: InlineBlock,
    },
    Failed {
        id: i64,
        error: String,
    },
    /// Deleted from the store meanwhile: not posted.
    Gone(i64),
    /// Written on an older version of the PR: not posted.
    Outdated(i64),
    /// Sent, outcome unknown (the batch stops).
    Unconfirmed(i64),
    Done,
    Resolved {
        root_id: CommentId,
        resolved: bool,
        result: Result<(), PostError>,
    },
}

/// What `Composer::poll` asks of the screen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollOutcome {
    /// Comments changed in a way only a refetch shows.
    pub refresh: bool,
    /// The server reported another head: the screen is stale.
    pub stale: bool,
    /// Threads were changed here (counts need updating).
    pub threads_changed: bool,
}

/// One slot of the stack under a diff row, in paint order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Thread(usize),
    Draft(i64),
    Editor,
}

/// Where drafts and the composer go on the current file's rows.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RowPlan {
    pub path: String,
    /// Row -> its stack (threads with their reply drafts, then inline drafts).
    pub rows: BTreeMap<usize, Vec<Slot>>,
    /// Inline drafts of this file whose line is not shown.
    pub unplaced: Vec<i64>,
    /// The composer is inline on this file but its line is not shown.
    pub editor_unplaced: bool,
}

/// What a click on the review screen asks for; applied by `ReviewScreen::act`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Open(Target),
    Edit(i64),
    Delete(i64),
    AsFile(i64),
    AsPr(i64),
    /// Show where a draft lives (file, conversation), then usually `Edit`.
    Reveal(i64),
    Save,
    Cancel,
    Submit,
    Confirm,
    CancelSubmit,
    Resolve(CommentId),
    Undo,
    DismissError,
    /// Stop the running batch after the comment being posted now.
    StopBatch,
    /// Refresh the comments to see whether an unconfirmed draft was posted.
    CheckComments(i64),
    /// "Post again anyway" on an unconfirmed draft (after Check comments).
    Rearm(i64),
    /// "Suggest change": the commented lines go into the composer as a suggestion.
    Suggest,
}

pub struct Composer {
    /// The forge is read-only in this version (GitHub in P1): no editor opens, no thread
    /// button and no Submit are shown.
    pub read_only: bool,
    /// The PR's forge: line ranges, suggestions and body rules (normalisation, quick actions).
    pub forge: Forge,
    pub(super) repo: RepoRef,
    pub(super) pr_id: u64,
    pub(super) store: Arc<Mutex<Store>>,
    /// The fetched head (`Opened`), stored with new drafts.
    pub head: String,
    /// The PR moved since it was fetched: inline drafts are held.
    pub stale: bool,
    /// Inline drafts may be posted (see `inline_posting_enabled`); else they stay saved.
    pub(super) inline_enabled: bool,
    pub(super) poster: Option<Poster>,
    pub(super) rows: Option<RowSource>,
    pub(super) ctx: Option<egui::Context>,
    /// Drafts of this PR, oldest first.
    pub drafts: Vec<Draft>,
    pub(super) states: HashMap<i64, DraftState>,
    pub editor: Option<Editor>,
    /// Some while the submit confirmation is open: the drafts it lists.
    pub confirm: Option<Vec<i64>>,
    pub(super) batch: bool,
    /// Ids of the running batch (for "Posting 2 of 5").
    pub(super) batch_ids: Vec<i64>,
    pub(super) tx: Sender<Event>,
    pub(super) rx: Receiver<Event>,
    /// Resolves waiting for the server: root id -> the state asked for.
    pub(super) resolving: HashMap<CommentId, bool>,
    pub toast: Option<Toast>,
    pub error: Option<String>,
    /// A resolve whose answer was lost: not a failure, the comments are refetched (amber).
    pub notice: Option<String>,
    /// Resolves with a lost answer: root id -> the state asked for, until comments arrive.
    pub(super) unconfirmed_resolves: HashMap<CommentId, bool>,
    /// Bumped on every change that can alter a draft card or the composer's height.
    pub(super) ver: u64,
    /// Computed in the layout pass, read when painting the rows.
    pub(super) plan: std::cell::RefCell<RowPlan>,
    pub(super) in_flight: InFlight,
    /// Set to stop the running batch after its current draft (Back, Reload, drop).
    pub(super) cancel: Arc<AtomicBool>,
    /// "Stop after this comment" was clicked for the running batch.
    pub(super) stopping: bool,
    /// Drafts the running batch has posted (for "Posting N of M").
    pub(super) batch_done: usize,
    /// Unconfirmed drafts whose "Check comments" waits for the refreshed comments.
    pub(super) checking: HashSet<i64>,
    /// Unconfirmed drafts: the text that was sent (the draft may be edited afterwards).
    pub(super) sent_text: HashMap<i64, String>,
    /// Test hook: this many store deletes fail.
    #[cfg(test)]
    pub(super) fail_deletes: Arc<AtomicUsize>,
    /// Test hook: this many tombstone writes fail.
    #[cfg(test)]
    pub(super) fail_marks: Arc<AtomicUsize>,
}

impl Composer {
    /// Loads this PR's drafts from the store (with a registry of its own; the app passes the
    /// shared one through `with_in_flight`).
    #[cfg(test)]
    pub fn new(repo: RepoRef, pr_id: u64, store: Arc<Mutex<Store>>) -> Composer {
        Composer::with_in_flight(repo, pr_id, store, InFlight::default())
    }

    /// `new`, using the process-wide in-flight registry from the first load on, so the
    /// tombstone cleanup never deletes a draft whose worker is still finishing it.
    pub fn with_in_flight(
        repo: RepoRef,
        pr_id: u64,
        store: Arc<Mutex<Store>>,
        reg: InFlight,
    ) -> Composer {
        let (tx, rx) = channel();
        let mut c = Composer {
            repo,
            pr_id,
            store,
            head: String::new(),
            stale: false,
            inline_enabled: inline_posting_enabled(),
            poster: None,
            read_only: false,
            forge: Forge::BitbucketCloud,
            rows: None,
            ctx: None,
            drafts: Vec::new(),
            states: HashMap::new(),
            editor: None,
            confirm: None,
            batch: false,
            batch_ids: Vec::new(),
            tx,
            rx,
            resolving: HashMap::new(),
            toast: None,
            error: None,
            notice: None,
            unconfirmed_resolves: HashMap::new(),
            ver: 0,
            plan: Default::default(),
            in_flight: reg,
            cancel: Arc::new(AtomicBool::new(false)),
            stopping: false,
            batch_done: 0,
            checking: HashSet::new(),
            sent_text: HashMap::new(),
            #[cfg(test)]
            fail_deletes: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            fail_marks: Arc::new(AtomicUsize::new(0)),
        };
        c.load();
        c
    }

    /// The batch was asked to stop after its current draft.
    pub fn stopping(&self) -> bool {
        self.batch && self.stopping
    }

    /// With the first `deletes` store deletes and `marks` tombstone writes failing.
    #[cfg(test)]
    pub(super) fn with_failing_store(
        repo: RepoRef,
        pr_id: u64,
        store: Arc<Mutex<Store>>,
        reg: InFlight,
        deletes: usize,
        marks: usize,
    ) -> Composer {
        let mut c = Composer::with_failing_deletes(repo, pr_id, store, deletes);
        c.in_flight = reg;
        c.fail_marks.store(marks, Ordering::SeqCst);
        c
    }

    /// `new`, with the first `n` store deletes failing (tests of the posted tombstone).
    #[cfg(test)]
    pub(super) fn with_failing_deletes(
        repo: RepoRef,
        pr_id: u64,
        store: Arc<Mutex<Store>>,
        n: usize,
    ) -> Composer {
        let scratch = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let mut c = Composer::new(repo, pr_id, scratch);
        c.store = store;
        c.fail_deletes.store(n, Ordering::SeqCst);
        c.drafts.clear();
        c.load();
        c
    }

    /// Reads this PR's drafts; posted ones (tombstones) are deleted now, or kept as
    /// "posted, cleaning up" and never offered again when the delete fails.
    pub(super) fn load(&mut self) {
        let loaded = self.lock().list_drafts(&self.repo, self.pr_id);
        let posted = self.lock().posted_draft_ids(&self.repo, self.pr_id);
        let posting = self.lock().posting_draft_ids(&self.repo, self.pr_id);
        let (mut ds, posted, posting) = match (loaded, posted, posting) {
            (Ok(ds), Ok(p), Ok(q)) => (ds, p, q),
            _ => {
                self.error = Some("Could not read the saved drafts".into());
                return;
            }
        };
        // sent earlier without a known outcome (a lost answer, or the app stopped): never
        // offered again silently; one this process is posting right now shows as posting
        for id in posting {
            if !self.is_in_flight(id) {
                self.states.entry(id).or_insert(DraftState::Unconfirmed {
                    checked: false,
                    looks_posted: false,
                });
                if let Some(d) = ds.iter().find(|d| d.id == id) {
                    self.sent_text.entry(id).or_insert_with(|| d.posted_text());
                }
            }
        }
        for id in posted {
            if !self.is_in_flight(id) && self.delete_stored(id) {
                ds.retain(|d| d.id != id);
            } else {
                self.states.insert(id, DraftState::PostedNotRemoved);
            }
        }
        ds.sort_by_key(|d| d.id);
        self.drafts = ds;
    }

    /// A resolve is waiting for the server (Back and Reload wait for it).
    pub fn any_resolving(&self) -> bool {
        !self.resolving.is_empty()
    }

    /// Posting, resolving and the inline check's rows; the context repaints on results.
    pub fn connect(&mut self, poster: Poster, rows: RowSource, ctx: Option<egui::Context>) {
        self.poster = Some(poster);
        self.rows = Some(rows);
        self.ctx = ctx;
    }

    /// Uses the process-wide in-flight registry (`ReviewHost` owns it).
    #[allow(dead_code)] // UI wiring pending (write-safety wave); covered by tests
    pub fn share_in_flight(&mut self, reg: InFlight) {
        self.in_flight = reg;
    }

    /// Stops the running batch after the draft being posted now; the rest stay drafts.
    pub fn cancel_batch(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.stopping = self.batch;
    }

    /// Edits of a draft some batch is posting right now are refused.
    pub(super) fn refuse_in_flight(&mut self, id: i64) -> bool {
        let busy = self.is_in_flight(id);
        if busy {
            self.error = Some("This draft is being posted; it cannot be changed now".into());
        }
        busy
    }

    /// Re-applies resolves still waiting for the server over freshly fetched threads.
    pub fn reapply_pending(&self, comments: &mut Comments) {
        for (root, &want) in &self.resolving {
            comments.set_resolved(root, want);
        }
    }

    pub(super) fn is_in_flight(&self, id: i64) -> bool {
        lock_ok(&self.in_flight).contains(&id)
    }

    pub(super) fn delete_stored(&self, id: i64) -> bool {
        #[cfg(test)]
        if self
            .fail_deletes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return false;
        }
        self.lock().delete_draft(id).is_ok()
    }

    pub fn can_post(&self) -> bool {
        self.poster.is_some()
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        lock_ok(&self.store)
    }

    pub(super) fn bump(&mut self) {
        self.ver += 1;
    }

    /// Changes whenever a draft card or the composer may need another height.
    pub fn version(&self) -> u64 {
        self.ver
    }

    pub fn state(&self, id: i64) -> DraftState {
        let own = self.states.get(&id).cloned().unwrap_or_default();
        // a marker read while another screen of this process was sending it is not news
        let quiet = matches!(own, DraftState::Idle | DraftState::Unconfirmed { .. });
        if quiet && self.is_in_flight(id) {
            return DraftState::Posting;
        }
        own
    }

    pub fn draft(&self, id: i64) -> Option<&Draft> {
        self.drafts.iter().find(|d| d.id == id)
    }

    /// A batch is posting: Submit and every draft edit are disabled.
    pub fn running(&self) -> bool {
        self.batch
    }

    /// "Posting 2 of 5..." while a batch runs.
    pub fn progress(&self) -> Option<String> {
        if !self.batch {
            return None;
        }
        // skipped drafts (blocked, outdated, gone, claimed elsewhere) leave `batch_ids`
        let n = self.batch_ids.len();
        let at = (self.batch_done + 1).min(n);
        Some(format!("Posting {at} of {n}..."))
    }

    pub fn is_resolving(&self, root_id: &CommentId) -> bool {
        self.resolving.contains_key(root_id)
    }
}
