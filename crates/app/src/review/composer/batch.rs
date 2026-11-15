//! The submit worker and the in-flight claim.

use super::*;

/// The submit worker: posts drafts in order, stops at the first failure.
pub(super) struct Batch {
    pub(super) in_flight: InFlight,
    /// Checked between drafts; the draft being posted is always finished (post + delete).
    pub(super) cancel: Arc<AtomicBool>,
    /// The head the screen was loaded at when the batch started.
    pub(super) head: String,
    /// Inline drafts are refused here too, whatever the caller listed.
    pub(super) inline_enabled: bool,
    /// Text that would run a GitLab quick action is refused here (`quick_action_guard`).
    pub(super) forge: Forge,
    pub(super) poster: Poster,
    pub(super) rows: Option<RowSource>,
    pub(super) store: Arc<Mutex<Store>>,
    pub(super) repo: RepoRef,
    pub(super) pr_id: u64,
    pub(super) tx: Sender<Event>,
    pub(super) ctx: Option<egui::Context>,
    #[cfg(test)]
    pub(super) fail_deletes: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(super) fail_marks: Arc<AtomicUsize>,
}

/// One draft's outcome: keep going or stop the batch.
pub(super) enum Step {
    Next,
    Stop,
}

impl Batch {
    pub(super) fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }

    pub(super) fn run(self, drafts: Vec<Draft>) {
        for d in drafts {
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            let step =
                std::panic::catch_unwind(AssertUnwindSafe(|| self.one(&d))).unwrap_or_else(|_| {
                    self.send(Event::Failed {
                        id: d.id,
                        error: "internal error".into(),
                    });
                    Step::Stop
                });
            if matches!(step, Step::Stop) {
                break;
            }
        }
        self.send(Event::Done);
    }

    pub(super) fn delete(&self, id: i64) -> bool {
        #[cfg(test)]
        if self
            .fail_deletes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return false;
        }
        let st = lock_ok(&self.store);
        st.delete_draft(id).is_ok()
    }

    pub(super) fn mark(&self, id: i64) -> bool {
        #[cfg(test)]
        if self
            .fail_marks
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return false;
        }
        let st = lock_ok(&self.store);
        st.mark_draft_posted(id).is_ok()
    }

    pub(super) fn fail(&self, id: i64, error: String) -> Step {
        self.send(Event::Failed { id, error });
        Step::Stop
    }

    pub(super) fn one(&self, d: &Draft) -> Step {
        // claimed for the whole process first: no other batch posts it meanwhile
        let Some(claim) = Claim::take(&self.in_flight, d.id) else {
            self.send(Event::Elsewhere(d.id));
            return Step::Next;
        };
        // only drafts still stored and not posted are posted (else: skipped)
        let lock = || lock_ok(&self.store);
        let present = {
            let st = lock();
            st.list_drafts(&self.repo, self.pr_id).and_then(|v| {
                let posted = st.posted_draft_ids(&self.repo, self.pr_id)?;
                let posting = st.posting_draft_ids(&self.repo, self.pr_id)?;
                Ok((
                    v.iter().any(|x| x.id == d.id) && !posted.contains(&d.id),
                    posting.contains(&d.id),
                ))
            })
        };
        match present {
            Ok((true, false)) => {}
            // sent before without a known outcome (another screen, an earlier run)
            Ok((true, true)) => {
                self.send(Event::Unconfirmed(d.id));
                return Step::Next;
            }
            Ok((false, _)) => {
                self.send(Event::Gone(d.id));
                return Step::Next;
            }
            Err(_) => return self.fail(d.id, "could not read the saved drafts".into()),
        }
        let Some(c) = new_comment(d) else {
            return self.fail(d.id, "the draft is incomplete".into());
        };
        if !self.inline_enabled && matches!(c, NewComment::Inline { .. }) {
            return self.fail(d.id, inline_disabled_message(1));
        }
        if let Err(why) = quick_action_guard(self.forge, &d.posted_text()) {
            return self.fail(d.id, why);
        }
        self.send(Event::Posting(d.id));
        // what was reviewed: the batch's head; an inline draft also pins its file's merge base
        let mut at = PostRevision {
            head: self.head.clone(),
            merge_base: String::new(),
        };
        if let NewComment::Inline {
            path,
            side,
            line,
            start,
            ..
        } = &c
        {
            let Some(rows) = &self.rows else {
                return self.fail(d.id, "the diff is not loaded".into());
            };
            let file = match rows(path) {
                Ok(f) => f,
                Err(e) => return self.fail(d.id, e),
            };
            // rows of the review this batch was started from, nothing else
            let same_review = file.repo == self.repo
                && file.pr_id == self.pr_id
                && same_commit_strict(&file.head, &self.head);
            if !same_review {
                return self.fail(
                    d.id,
                    "the review was reloaded since posting started; submit again".into(),
                );
            }
            // a line number only means this code on the version it was written on
            if !same_commit_strict(&d.source_commit, &file.head) {
                self.send(Event::Outdated(d.id));
                return Step::Next;
            }
            let now = match self.poster.current_revision(&self.repo, self.pr_id) {
                Ok(r) => r,
                Err(e) => {
                    return self.fail(d.id, format!("could not check the PR: {}", e.message()))
                }
            };
            // compared per anchor model (spec 7.3): Bitbucket's current diff needs the same
            // head; other models also need the same base. The destination branch the review
            // was fetched against is not tracked yet, so a head-commit model is refused.
            let server = ServerRevision {
                head: head_for_check(&now.head, &file.head),
                ..now
            };
            let local = PostRevision {
                head: file.head.clone(),
                merge_base: file.merge_base.clone(),
            };
            let check = inline_allowed(
                self.poster.anchor_model(&self.repo),
                &server,
                &local,
                "",
                &file.rows,
                file.binary,
                *side,
                *line,
                *start,
            );
            if let Err(block) = check {
                self.send(Event::Blocked { id: d.id, block });
                return Step::Next;
            }
            at = local;
        }
        // recorded before sending: a crash or a lost answer from here on can never make the
        // draft look "not posted" (it shows as unconfirmed, also after a restart)
        if lock().mark_draft_posting(d.id).is_err() {
            return self.fail(
                d.id,
                "could not record the draft locally; not posted".into(),
            );
        }
        let comment = match self.poster.post(&self.repo, self.pr_id, &at, &c) {
            Ok(comment) => Some(Box::new(comment)),
            Err(PostError::PostedUnreadable) => None,
            Err(PostError::Unconfirmed) => {
                self.send(Event::Unconfirmed(d.id));
                return Step::Stop;
            }
            Err(e) => {
                // definitely not posted: offered again (a failed clear errs on the safe side)
                let _ = lock().clear_draft_posting(d.id);
                return self.fail(d.id, e.message());
            }
        };
        // the server has it: tombstone first (never posted again, even after a restart),
        // then delete (one retry); never before the 2xx
        let recorded = (0..3u64).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50 * attempt));
            }
            self.mark(d.id)
        });
        let removed = self.delete(d.id) || self.delete(d.id);
        if !recorded && !removed {
            // Only this process knows the server has it: the claim is leaked on purpose, so
            // no screen of this process offers the draft again (a restart would; the error
            // shown says not to submit it).
            std::mem::forget(claim);
        }
        self.send(Event::Posted {
            id: d.id,
            comment,
            removed,
            recorded,
        });
        Step::Next
    }
}

/// A draft id held in the in-flight registry until dropped.
pub(super) struct Claim {
    pub(super) reg: InFlight,
    pub(super) id: i64,
}

impl Claim {
    pub(super) fn take(reg: &InFlight, id: i64) -> Option<Claim> {
        let fresh = lock_ok(reg).insert(id);
        fresh.then(|| Claim {
            reg: reg.clone(),
            id,
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        lock_ok(&self.reg).remove(&self.id);
    }
}

impl Drop for Composer {
    /// Back / Reload drop the screen: a running batch finishes its current draft and stops.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}
