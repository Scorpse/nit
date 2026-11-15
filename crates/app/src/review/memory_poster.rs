//! In-memory `CommentPoster` for `--demo` and tests: keeps posts, answers with synthetic
//! comments and can be scripted to fail or wait.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use prcore::comments::{
    Anchor, AnchorModel, Comment, CommentId, NewComment, PostRevision, ReviewAction,
    ServerRevision, ThreadId,
};
use prcore::draft::Side;
use prcore::model::{PullRequest, RepoRef, ReviewState, Reviewer, UserRef};

use super::composer::{CommentPoster, PostError};
use crate::lock::lock_ok;

/// Keeps posts in memory and answers with synthetic comments (`--demo`, tests). `script`
/// holds the outcome of the next posts (`None`: success); empty means success.
pub struct MemoryPoster {
    pub head: Mutex<String>,
    /// Comments the "server" accepted, in order (unreadable answers included).
    pub posted: Mutex<Vec<NewComment>>,
    /// Every post call, accepted or not.
    pub attempts: Mutex<Vec<NewComment>>,
    /// The revision each post call carried, in order.
    pub revisions: Mutex<Vec<PostRevision>>,
    pub resolves: Mutex<Vec<(ThreadId, bool)>>,
    pub script: Mutex<VecDeque<Option<PostError>>>,
    pub resolve_script: Mutex<VecDeque<Option<PostError>>>,
    /// Every `set_review` call, in order.
    pub reviews: Mutex<Vec<ReviewAction>>,
    /// The body sent with each `set_review` call, in order.
    pub review_bodies: Mutex<Vec<Option<String>>>,
    pub review_script: Mutex<VecDeque<Option<PostError>>>,
    /// `set_review` waits while this is set.
    pub hold_review: Mutex<bool>,
    /// A post of exactly this text waits until `release` (tests of in-flight posts).
    pub hold: Mutex<Option<String>>,
    /// Resolves wait while this is set.
    pub hold_resolve: Mutex<bool>,
    /// Outcome of the next `current_head` / `current_pr` calls (`None`: the head).
    pub head_script: Mutex<VecDeque<Option<PostError>>>,
    /// Every `current_head` call is counted here.
    pub head_calls: AtomicU64,
    /// The head becomes this after the next successful `set_review` (pushed meanwhile).
    pub head_after_review: Mutex<Option<String>>,
    /// The PR `current_pr` answers with (head from `head`); None: not found.
    pub pr: Mutex<Option<PullRequest>>,
    /// Who `set_review` acts as on `pr`'s reviewers (None: they are left alone).
    pub me: Mutex<Option<UserRef>>,
    /// The forge's anchor model (Bitbucket's by default).
    pub anchor: Mutex<AnchorModel>,
    /// The merge base `current_revision` reports (None: not reported).
    pub server_base: Mutex<Option<String>>,
    held: Condvar,
    next_id: AtomicU64,
}

impl MemoryPoster {
    pub fn new(head: &str) -> MemoryPoster {
        MemoryPoster {
            head: Mutex::new(head.to_string()),
            posted: Mutex::default(),
            attempts: Mutex::default(),
            revisions: Mutex::default(),
            resolves: Mutex::default(),
            script: Mutex::default(),
            resolve_script: Mutex::default(),
            reviews: Mutex::default(),
            review_bodies: Mutex::default(),
            review_script: Mutex::default(),
            hold_review: Mutex::default(),
            hold: Mutex::default(),
            hold_resolve: Mutex::default(),
            head_script: Mutex::default(),
            head_calls: AtomicU64::new(0),
            head_after_review: Mutex::default(),
            pr: Mutex::default(),
            me: Mutex::default(),
            anchor: Mutex::new(AnchorModel::CurrentDiff),
            server_base: Mutex::default(),
            held: Condvar::new(),
            next_id: AtomicU64::new(100_000),
        }
    }

    /// Holds posts of `text` (None: none), releasing any other held post.
    #[cfg(test)]
    pub fn hold_text(&self, text: Option<&str>) {
        *lock_ok(&self.hold) = text.map(str::to_string);
        self.held.notify_all();
    }

    /// Lets held posts and resolves go on.
    #[cfg(test)]
    pub fn release(&self) {
        *lock_ok(&self.hold) = None;
        *lock_ok(&self.hold_resolve) = false;
        *lock_ok(&self.hold_review) = false;
        self.held.notify_all();
    }
}

impl CommentPoster for MemoryPoster {
    fn post(
        &self,
        _repo: &RepoRef,
        _pr_id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, PostError> {
        lock_ok(&self.attempts).push(c.clone());
        lock_ok(&self.revisions).push(at.clone());
        let text = match c {
            NewComment::Pr { text }
            | NewComment::File { text, .. }
            | NewComment::Inline { text, .. }
            | NewComment::Reply { text, .. } => text.clone(),
        };
        let mut h = lock_ok(&self.hold);
        while h.as_deref() == Some(text.as_str()) {
            h = self.held.wait(h).unwrap_or_else(|p| p.into_inner());
        }
        drop(h);
        let outcome = lock_ok(&self.script).pop_front().flatten();
        match outcome {
            // the "server" got both; only the answer is unreadable or lost
            Some(e @ (PostError::PostedUnreadable | PostError::Unconfirmed)) => {
                lock_ok(&self.posted).push(c.clone());
                return Err(e);
            }
            Some(e) => return Err(e),
            None => {}
        }
        lock_ok(&self.posted).push(c.clone());
        let now = chrono::Utc::now();
        let (body, parent_id, anchor) = match c {
            NewComment::Pr { text } => (text, None, None),
            NewComment::File { path, text } => (
                text,
                None,
                Some(Anchor {
                    path: path.clone(),
                    from: None,
                    to: None,
                    start: None,
                    outdated: false,
                    old_path: None,
                    commit: None,
                }),
            ),
            NewComment::Inline {
                path,
                side,
                line,
                start,
                text,
            } => (
                text,
                None,
                Some(Anchor {
                    path: path.clone(),
                    from: (*side == Side::Old).then_some(*line),
                    to: (*side == Side::New).then_some(*line),
                    start: *start,
                    outdated: false,
                    old_path: None,
                    commit: None,
                }),
            ),
            NewComment::Reply { thread, text } => (text, Some(CommentId(thread.0.clone())), None),
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(Comment {
            id: id.into(),
            thread: Some(ThreadId::from(id)),
            parent_id,
            can_resolve: true,
            author: UserRef {
                id: "{demo-you}".into(),
                display_name: "You".into(),
                login: None,
            },
            body: body.clone(),
            created_on: now,
            updated_on: now,
            deleted: false,
            resolved: false,
            anchor,
            author_is_bot: false,
        })
    }

    fn resolve(
        &self,
        _repo: &RepoRef,
        _pr_id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), PostError> {
        lock_ok(&self.resolves).push((thread.clone(), resolved));
        let mut h = lock_ok(&self.hold_resolve);
        while *h {
            h = self.held.wait(h).unwrap_or_else(|p| p.into_inner());
        }
        drop(h);
        match lock_ok(&self.resolve_script).pop_front().flatten() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn anchor_model(&self, _repo: &RepoRef) -> AnchorModel {
        *lock_ok(&self.anchor)
    }

    fn current_revision(&self, repo: &RepoRef, pr_id: u64) -> Result<ServerRevision, PostError> {
        let head = self.current_head(repo, pr_id)?;
        Ok(ServerRevision {
            head,
            base: lock_ok(&self.server_base).clone(),
            start: None,
            dest_branch: "main".into(),
        })
    }

    fn current_head(&self, _repo: &RepoRef, _pr_id: u64) -> Result<String, PostError> {
        self.head_calls.fetch_add(1, Ordering::SeqCst);
        match lock_ok(&self.head_script).pop_front().flatten() {
            Some(e) => Err(e),
            None => Ok(lock_ok(&self.head).clone()),
        }
    }

    fn current_pr(&self, _repo: &RepoRef, _pr_id: u64) -> Result<PullRequest, PostError> {
        let mut pr = lock_ok(&self.pr)
            .clone()
            .ok_or_else(|| PostError::Rejected("not found".into()))?;
        pr.source_commit = lock_ok(&self.head).clone();
        Ok(pr)
    }

    fn set_review(
        &self,
        _repo: &RepoRef,
        _pr_id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), PostError> {
        lock_ok(&self.reviews).push(a);
        lock_ok(&self.review_bodies).push(body.map(str::to_string));
        let mut h = lock_ok(&self.hold_review);
        while *h {
            h = self.held.wait(h).unwrap_or_else(|p| p.into_inner());
        }
        drop(h);
        // a lost answer (Unconfirmed, PostedUnreadable): the "server" applied it anyway
        let lost = match lock_ok(&self.review_script).pop_front().flatten() {
            Some(e @ (PostError::PostedUnreadable | PostError::Unconfirmed)) => Some(e),
            Some(e) => return Err(e),
            None => None,
        };
        if let Some(next) = lock_ok(&self.head_after_review).take() {
            *lock_ok(&self.head) = next;
        }
        let me = lock_ok(&self.me).clone();
        if let (Some(me), Some(pr)) = (me, lock_ok(&self.pr).as_mut()) {
            let state = match a {
                ReviewAction::Approve => ReviewState::Approved,
                ReviewAction::RequestChanges => ReviewState::ChangesRequested,
                ReviewAction::ClearApproval | ReviewAction::ClearRequestChanges => {
                    ReviewState::Pending
                }
            };
            match pr.reviewers.iter_mut().find(|r| r.user.id == me.id) {
                Some(r) => r.state = state,
                None => pr.reviewers.push(Reviewer { user: me, state }),
            }
        }
        lost.map_or(Ok(()), Err)
    }
}
