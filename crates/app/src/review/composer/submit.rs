//! Confirmed sequential submit, resolve / unresolve and polling the worker.

use super::*;

impl Composer {
    // ---- submit ----

    /// Drafts a submit would post now, oldest first. Inline drafts are held while stale.
    pub fn postable(&self) -> Vec<i64> {
        self.drafts
            .iter()
            .filter(|d| {
                !matches!(
                    self.state(d.id),
                    DraftState::Posting
                        | DraftState::PostedNotRemoved
                        | DraftState::Blocked(_)
                        | DraftState::Outdated
                        | DraftState::Unconfirmed { .. }
                )
            })
            .filter(|d| !(self.stale && d.kind == DraftKind::Inline))
            .filter(|d| self.inline_enabled || d.kind != DraftKind::Inline)
            .filter(|d| !self.is_in_flight(d.id))
            .map(|d| d.id)
            .collect()
    }

    /// Inline posting is on (verified, or the experiment flag).
    pub fn inline_enabled(&self) -> bool {
        self.inline_enabled
    }

    /// Sets the PR's forge and its inline gate (`inline_posting_enabled_for`).
    pub fn set_forge(&mut self, forge: Forge) {
        self.forge = forge;
        self.inline_enabled = inline_posting_enabled_for(forge);
    }

    /// Inline drafts a submit refuses because inline posting is disabled.
    pub fn refused_inline(&self) -> usize {
        if self.inline_enabled {
            return 0;
        }
        self.drafts
            .iter()
            .filter(|d| d.kind == DraftKind::Inline)
            .count()
    }

    /// Inline drafts held back because the PR changed.
    pub fn held_inline(&self) -> usize {
        if !self.stale {
            return 0;
        }
        self.drafts
            .iter()
            .filter(|d| d.kind == DraftKind::Inline)
            .count()
    }

    /// "Submit drafts": opens the confirmation; nothing is posted before `confirm_submit`.
    pub fn request_submit(&mut self) {
        if self.batch {
            return;
        }
        if self.poster.is_none() {
            self.error = Some("Posting is unavailable: sign in first".into());
            return;
        }
        self.stash_editor();
        // other screens may have posted or deleted drafts meanwhile
        let states = std::mem::take(&mut self.states);
        self.load();
        for (id, st) in states {
            if matches!(st, DraftState::Unconfirmed { .. }) {
                // still unconfirmed in the store: keep what this screen knows (checked);
                // re-armed or settled elsewhere: the reload is right
                if let Some(now @ DraftState::Unconfirmed { .. }) = self.states.get_mut(&id) {
                    *now = st;
                }
            } else {
                self.states.entry(id).or_insert(st);
            }
        }
        let refused = self.refused_inline();
        if refused > 0 {
            self.error = Some(inline_disabled_message(refused));
        }
        let ids = self.postable();
        if !ids.is_empty() {
            self.confirm = Some(ids);
        }
        self.bump();
    }

    pub fn cancel_submit(&mut self) {
        self.confirm = None;
    }

    /// The confirmation's "Post" button: posts the listed drafts that are still postable,
    /// one at a time on a worker thread. Returns whether a batch started.
    pub fn confirm_submit(&mut self) -> bool {
        let Some(listed) = self.confirm.take() else {
            return false;
        };
        if self.batch {
            return false;
        }
        let Some(poster) = self.poster.clone() else {
            return false;
        };
        let now = self.postable();
        let drafts: Vec<Draft> = self
            .drafts
            .iter()
            .filter(|d| listed.contains(&d.id) && now.contains(&d.id))
            .cloned()
            .collect();
        if drafts.is_empty() {
            return false;
        }
        self.batch = true;
        self.batch_ids = drafts.iter().map(|d| d.id).collect();
        self.error = match self.refused_inline() {
            0 => None,
            n => Some(inline_disabled_message(n)),
        };
        self.cancel = Arc::new(AtomicBool::new(false));
        self.stopping = false;
        self.batch_done = 0;
        let job = Batch {
            in_flight: self.in_flight.clone(),
            cancel: self.cancel.clone(),
            head: self.head.clone(),
            inline_enabled: self.inline_enabled,
            forge: self.forge,
            poster,
            rows: self.rows.clone(),
            store: self.store.clone(),
            repo: self.repo.clone(),
            pr_id: self.pr_id,
            tx: self.tx.clone(),
            ctx: self.ctx.clone(),
            #[cfg(test)]
            fail_deletes: self.fail_deletes.clone(),
            #[cfg(test)]
            fail_marks: self.fail_marks.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name("review-submit".into())
            .spawn(move || job.run(drafts));
        if spawned.is_err() {
            self.batch = false;
            self.error = Some("Could not start a background thread".into());
            return false;
        }
        self.bump();
        true
    }

    // ---- resolve ----

    /// Resolve / Unresolve on a thread card: shown at once, undone if the server refuses.
    pub fn toggle_resolved(&mut self, comments: &mut Comments, root_id: &CommentId) {
        let Some(now) = comments
            .threads
            .iter()
            .find(|t| t.root.id == *root_id)
            .map(|t| t.root.resolved)
        else {
            return;
        };
        self.resolve_to(comments, root_id.clone(), !now, true);
    }

    /// The toast's Undo: flips the thread back (no further undo).
    pub fn undo(&mut self, comments: &mut Comments) {
        let Some(t) = &self.toast else { return };
        if !t.can_undo || self.resolving.contains_key(&t.root_id) {
            return;
        }
        // back to what it was before the click, whatever a refresh shows now
        let (root, want) = (t.root_id.clone(), !t.resolved);
        self.toast = None;
        self.resolve_to(comments, root, want, false);
    }

    pub(super) fn resolve_to(
        &mut self,
        comments: &mut Comments,
        root_id: CommentId,
        want: bool,
        can_undo: bool,
    ) {
        if self.resolving.contains_key(&root_id) {
            return;
        }
        let Some(poster) = self.poster.clone() else {
            self.error = Some("Resolving is unavailable: sign in first".into());
            return;
        };
        let Some((now, thread)) = comments
            .threads
            .iter()
            .find(|t| t.root.id == root_id)
            .map(|t| (t.root.resolved, t.id().cloned()))
        else {
            return;
        };
        let Some(thread) = thread else {
            self.error = Some("This thread cannot be resolved".into());
            return;
        };
        comments.set_resolved(&root_id, want);
        self.resolving.insert(root_id.clone(), want);
        self.toast = Some(Toast {
            root_id: root_id.clone(),
            resolved: want,
            at: Instant::now(),
            can_undo,
        });
        let (repo, pr_id, tx, ctx) = (
            self.repo.clone(),
            self.pr_id,
            self.tx.clone(),
            self.ctx.clone(),
        );
        let sent_root = root_id.clone();
        let spawned = std::thread::Builder::new()
            .name("review-resolve".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    poster.resolve(&repo, pr_id, &thread, want)
                }))
                .unwrap_or_else(|_| Err(PostError::Rejected("internal error".into())));
                let _ = tx.send(Event::Resolved {
                    root_id: sent_root,
                    resolved: want,
                    result,
                });
                if let Some(c) = ctx {
                    c.request_repaint();
                }
            });
        if spawned.is_err() {
            self.resolving.remove(&root_id);
            comments.set_resolved(&root_id, now);
            self.toast = None;
            self.error = Some("Could not start a background thread".into());
        }
    }

    /// Freshly fetched comments (the server's state, already shown): resolves whose answer
    /// was lost are settled against them. Applied: the note goes; not applied: it says so.
    pub fn resolves_checked(&mut self, comments: &Comments) {
        if self.unconfirmed_resolves.is_empty() {
            return;
        }
        let mut missed = false;
        for (root, want) in std::mem::take(&mut self.unconfirmed_resolves) {
            let now = comments
                .threads
                .iter()
                .find(|t| t.root.id == root)
                .map(|t| t.root.resolved);
            if now != Some(want) {
                missed = true;
                if self.toast.as_ref().is_some_and(|t| t.root_id == root) {
                    self.toast = None;
                }
            }
        }
        self.notice = missed.then(|| RESOLVE_REFRESHED.into());
        self.bump();
    }

    // ---- results ----

    /// Applies finished work (posts, blocks, failures, resolves) and expires the toast.
    pub fn poll(&mut self, comments: &mut Comments) -> PollOutcome {
        let mut out = PollOutcome::default();
        while let Ok(ev) = self.rx.try_recv() {
            self.bump();
            match ev {
                Event::Posting(id) => {
                    self.states.insert(id, DraftState::Posting);
                }
                Event::Posted {
                    id,
                    comment,
                    removed,
                    recorded,
                } => {
                    self.batch_done += 1;
                    if !removed && !recorded {
                        let which = self.draft(id).map(describe).unwrap_or_default();
                        self.error = Some(format!(
                            "This comment was posted but could not be recorded locally; do not \
                             submit it again ({which})."
                        ));
                    }
                    if removed {
                        self.drafts.retain(|d| d.id != id);
                        self.states.remove(&id);
                    } else {
                        self.states.insert(id, DraftState::PostedNotRemoved);
                    }
                    match comment {
                        Some(c) => {
                            comments.add_posted(*c);
                            out.threads_changed = true;
                        }
                        None => out.refresh = true,
                    }
                }
                Event::Blocked { id, block } => {
                    self.states.insert(id, DraftState::Blocked(block));
                    self.batch_ids.retain(|x| *x != id);
                    if block == InlineBlock::HeadMismatch {
                        self.stale = true;
                        out.stale = true;
                    }
                }
                Event::Failed { id, error } => {
                    self.states.insert(id, DraftState::Failed(error));
                }
                Event::Gone(id) => {
                    self.drafts.retain(|d| d.id != id);
                    self.states.remove(&id);
                    self.batch_ids.retain(|x| *x != id);
                }
                Event::Outdated(id) => {
                    self.states.insert(id, DraftState::Outdated);
                    self.batch_ids.retain(|x| *x != id);
                }
                Event::Elsewhere(id) => {
                    self.batch_ids.retain(|x| *x != id);
                }
                Event::Unconfirmed(id) => {
                    self.batch_ids.retain(|x| *x != id);
                    // drafts are not editable while the batch runs: this is the text sent
                    if let Some(d) = self.draft(id) {
                        self.sent_text.insert(id, d.posted_text());
                    }
                    self.states.insert(
                        id,
                        DraftState::Unconfirmed {
                            checked: false,
                            looks_posted: false,
                        },
                    );
                }
                Event::Done => {
                    self.batch = false;
                    self.stopping = false;
                    self.batch_ids.clear();
                }
                Event::Resolved {
                    root_id,
                    resolved,
                    result,
                } => {
                    self.resolving.remove(&root_id);
                    match result {
                        Ok(()) => {}
                        // sent, answer lost: it may have applied; the server's state is
                        // fetched and adopted (`resolves_checked`), not shown as failed
                        Err(PostError::Unconfirmed | PostError::PostedUnreadable) => {
                            self.unconfirmed_resolves.insert(root_id.clone(), resolved);
                            self.notice = Some(RESOLVE_REFRESHING.into());
                            out.refresh = true;
                        }
                        Err(e) => {
                            comments.set_resolved(&root_id, !resolved);
                            let verb = if resolved { "resolve" } else { "unresolve" };
                            self.error =
                                Some(format!("Could not {verb} the thread: {}", e.message()));
                            if self.toast.as_ref().is_some_and(|t| t.root_id == root_id) {
                                self.toast = None;
                            }
                        }
                    }
                    out.threads_changed = true;
                }
            }
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|t| t.at.elapsed() >= UNDO_FOR)
        {
            self.toast = None;
        }
        out
    }
}
