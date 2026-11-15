//! Review actions on the review screen's top bar: Approve / Request changes (each toggles
//! to its clear form when I already set it), their confirmation with stack-aware warnings,
//! and "Open in Bitbucket".
//!
//! `ReviewActions` is the testable state. Every action needs a confirmation; a confirmed
//! click runs on a worker thread, one at a time: it first re-reads the PR's head and sends
//! nothing when that moved since the screen fetched it (or cannot be read), then makes
//! exactly one review call and reads the PR back for its reviewers. The PR's reviewer list
//! is updated at once and put back if nothing was sent or the server refuses.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, Color32, Id, RichText};
use prcore::comments::{approval_warnings, ReviewAction};
use prcore::commit::same_commit;
use prcore::model::{Forge, PullRequest, RepoRef, ReviewState, Reviewer, UserRef};
use prcore::Stack;

use super::composer::{PostError, Poster};
use crate::theme::{AMBER, GREY};

/// Why Approve / Request changes are disabled while the screen is stale.
pub const STALE_ACTIONS: &str = "The PR changed since you opened it. Reload before approving.";

/// Shown next to the buttons when my Bitbucket user is not known.
pub const UNKNOWN_ME: &str =
    "Your Bitbucket user is not known yet, so whether you already approved or requested \
     changes cannot be shown.";

/// What the inbox knew when the review was opened: who I am and the PR's stack.
#[derive(Clone, Debug, Default)]
pub struct ReviewContext {
    /// None when my identity is unknown (never signed in, offline first run).
    pub me: Option<UserRef>,
    /// The stack the PR belongs to, if any.
    pub stack: Option<Stack>,
    /// PRs of the PR's repository by id (stack parents for the warnings).
    pub prs: HashMap<u64, PullRequest>,
}

/// My review state on `pr`; None when my identity is unknown. Not listed means Pending.
pub fn my_state(pr: &PullRequest, me: Option<&UserRef>) -> Option<ReviewState> {
    let me = me?;
    Some(
        pr.reviewers
            .iter()
            .find(|r| r.user.id == me.id)
            .map_or(ReviewState::Pending, |r| r.state.clone()),
    )
}

/// The two top-bar buttons for my current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buttons {
    /// `Approve` or `ClearApproval`.
    pub approve: ReviewAction,
    /// `RequestChanges` or `ClearRequestChanges`.
    pub request: ReviewAction,
    /// Shown next to the buttons (identity unknown).
    pub note: Option<&'static str>,
}

pub fn buttons(state: Option<ReviewState>) -> Buttons {
    let (approve, request) = match state {
        Some(ReviewState::Approved) => (ReviewAction::ClearApproval, ReviewAction::RequestChanges),
        Some(ReviewState::ChangesRequested) => {
            (ReviewAction::Approve, ReviewAction::ClearRequestChanges)
        }
        Some(ReviewState::Pending | ReviewState::Commented) | None => {
            (ReviewAction::Approve, ReviewAction::RequestChanges)
        }
    };
    Buttons {
        approve,
        request,
        note: state.is_none().then_some(UNKNOWN_ME),
    }
}

pub fn label(a: ReviewAction) -> &'static str {
    match a {
        ReviewAction::Approve => "Approve",
        ReviewAction::RequestChanges => "Request changes",
        ReviewAction::ClearApproval => "Remove approval",
        ReviewAction::ClearRequestChanges => "Withdraw request",
    }
}

/// The confirmation's heading.
pub fn question(a: ReviewAction) -> &'static str {
    match a {
        ReviewAction::Approve => "Approve this pull request?",
        ReviewAction::RequestChanges => "Request changes on this pull request?",
        ReviewAction::ClearApproval => "Remove your approval?",
        ReviewAction::ClearRequestChanges => "Withdraw your request for changes?",
    }
}

fn verb(a: ReviewAction) -> &'static str {
    match a {
        ReviewAction::Approve => "approve",
        ReviewAction::RequestChanges => "request changes",
        ReviewAction::ClearApproval => "remove the approval",
        ReviewAction::ClearRequestChanges => "withdraw the request for changes",
    }
}

fn done_text(a: ReviewAction) -> &'static str {
    match a {
        ReviewAction::Approve => "Approved.",
        ReviewAction::RequestChanges => "Changes requested.",
        ReviewAction::ClearApproval => "Approval removed.",
        ReviewAction::ClearRequestChanges => "Request for changes withdrawn.",
    }
}

/// My state once `a` went through.
fn state_after(a: ReviewAction) -> ReviewState {
    match a {
        ReviewAction::Approve => ReviewState::Approved,
        ReviewAction::RequestChanges => ReviewState::ChangesRequested,
        ReviewAction::ClearApproval | ReviewAction::ClearRequestChanges => ReviewState::Pending,
    }
}

/// The open confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub action: ReviewAction,
    /// Amber lines shown before the confirm button; they never block.
    pub warnings: Vec<String>,
    /// My other review state is set and stays as it is (one provider call per click).
    pub note: Option<String>,
    /// The review text typed in the dialog (asked for where `body_required`).
    pub body: String,
}

/// Shown when a request for changes on GitHub has no text.
pub const BODY_NEEDED: &str = "GitHub needs a comment to request changes; nothing was sent.";

/// Whether the forge refuses `a` without a review text: GitHub's REQUEST_CHANGES.
pub fn body_required(forge: Forge, a: ReviewAction) -> bool {
    forge == Forge::GitHub && a == ReviewAction::RequestChanges
}

/// How a confirmed action ended on the worker.
enum Outcome {
    /// The PR's head moved since the screen fetched it: nothing was sent.
    Moved,
    /// The head could not be checked: nothing was sent.
    Unchecked(String),
    /// The one provider call ran. `after` is the PR as the server sees it right after
    /// (None if it could not be read, or not read after a refusal).
    Sent {
        result: Result<(), PostError>,
        after: Option<Box<PullRequest>>,
    },
}

struct Done {
    action: ReviewAction,
    outcome: Outcome,
}

/// Shown when the head check before an action failed.
pub const UNCHECKED: &str = "Could not confirm the PR is unchanged, so nothing was sent";

/// The action's answer was lost and the PR, read again, does not show it applied.
pub const UNCONFIRMED_REFRESHED: &str = "Not confirmed: the connection failed after sending. \
     The reviewers were refreshed from Bitbucket and are shown as they are there now.";

/// The action's answer was lost and the PR could not be read again either.
pub const UNCONFIRMED_UNREAD: &str = "Not confirmed: the connection failed after sending, \
     and the pull request could not be refreshed. Reload to see whether it applied.";

pub struct ReviewActions {
    pub ctx: ReviewContext,
    /// The forge is read-only in this version (GitHub in P1): no action can be requested.
    pub read_only: bool,
    poster: Option<Poster>,
    repaint: Option<egui::Context>,
    pub confirm: Option<Confirm>,
    /// The action being sent and the reviewer list from before it (for the revert).
    in_flight: Option<(ReviewAction, Vec<Reviewer>)>,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    pub error: Option<String>,
    pub done: Option<String>,
    /// The answer to the action was lost: whether it applied is not known (amber).
    pub unconfirmed: Option<String>,
    /// The reviewers after the last action that went through, for the inbox's copy.
    updated: Option<Vec<Reviewer>>,
}

impl ReviewActions {
    pub fn new(ctx: ReviewContext) -> ReviewActions {
        let (tx, rx) = channel();
        ReviewActions {
            ctx,
            read_only: false,
            poster: None,
            repaint: None,
            confirm: None,
            in_flight: None,
            tx,
            rx,
            error: None,
            done: None,
            unconfirmed: None,
            updated: None,
        }
    }

    pub fn connect(&mut self, poster: Poster, repaint: Option<egui::Context>) {
        self.poster = Some(poster);
        self.repaint = repaint;
    }

    #[allow(dead_code)] // UI wiring pending (write-safety wave); covered by tests
    pub fn can_post(&self) -> bool {
        self.poster.is_some()
    }

    /// The reviewers after an action that went through (or was found applied), once.
    pub fn take_reviewers_update(&mut self) -> Option<Vec<Reviewer>> {
        self.updated.take()
    }

    /// An action is waiting for the server: both buttons are disabled.
    pub fn running(&self) -> bool {
        self.in_flight.is_some()
    }

    pub fn buttons(&self, pr: &PullRequest) -> Buttons {
        buttons(my_state(pr, self.ctx.me.as_ref()))
    }

    /// Why the buttons are disabled, if they are.
    pub fn disabled_reason(&self, stale: bool) -> Option<&'static str> {
        if self.read_only {
            Some(crate::review::composer::READ_ONLY_TIP)
        } else if stale {
            Some(STALE_ACTIONS)
        } else if self.running() {
            Some("Sending your review...")
        } else if self.poster.is_none() {
            Some("Reviewing needs a signed-in account")
        } else {
            None
        }
    }

    /// A button click: opens the confirmation (nothing is sent before `confirm`).
    pub fn request(
        &mut self,
        a: ReviewAction,
        pr: &PullRequest,
        stale: bool,
        unresolved: usize,
        drafts: usize,
    ) {
        if self.disabled_reason(stale).is_some() {
            return;
        }
        let mut warnings = Vec::new();
        if a == ReviewAction::Approve {
            if let Some(s) = &self.ctx.stack {
                warnings.extend(approval_warnings(s, &self.ctx.prs, pr.id));
            }
            if unresolved > 0 {
                let noun = if unresolved == 1 { "thread" } else { "threads" };
                warnings.push(format!("{unresolved} unresolved comment {noun}."));
            }
            if drafts > 0 {
                let noun = if drafts == 1 { "draft" } else { "drafts" };
                warnings.push(format!(
                    "{drafts} unsent comment {noun}: submit them first?"
                ));
            }
        }
        let mine = my_state(pr, self.ctx.me.as_ref());
        let note = match (a, mine) {
            (ReviewAction::Approve, Some(ReviewState::ChangesRequested)) => {
                Some("You also requested changes; that is not withdrawn by approving.".to_string())
            }
            (ReviewAction::RequestChanges, Some(ReviewState::Approved)) => Some(
                "You also approved; the approval is not removed by requesting changes.".to_string(),
            ),
            _ => None,
        };
        self.error = None;
        self.done = None;
        self.unconfirmed = None;
        self.confirm = Some(Confirm {
            action: a,
            warnings,
            note,
            body: String::new(),
        });
    }

    pub fn cancel(&mut self) {
        self.confirm = None;
    }

    /// The confirmation's button: sets my state on `pr` at once and, on a worker thread,
    /// first checks that the server's head is still `head` (the one this screen fetched),
    /// then sends the one provider call. Returns whether the worker started.
    pub fn confirm(
        &mut self,
        repo: &RepoRef,
        pr: &mut PullRequest,
        head: &str,
        stale: bool,
    ) -> bool {
        let Some(c) = self.confirm.take() else {
            return false;
        };
        if self.disabled_reason(stale).is_some() {
            return false;
        }
        if head.trim().len() < 7 {
            self.error = Some("The pull request is still loading; try again.".into());
            return false;
        }
        if body_required(pr.forge, c.action) && c.body.trim().is_empty() {
            self.error = Some(BODY_NEEDED.into());
            self.confirm = Some(c);
            return false;
        }
        let Some(poster) = self.poster.clone() else {
            return false;
        };
        let a = c.action;
        let body = (!c.body.trim().is_empty()).then(|| c.body.clone());
        let before = pr.reviewers.clone();
        if let Some(me) = &self.ctx.me {
            set_state(pr, me, state_after(a));
        }
        self.in_flight = Some((a, before.clone()));
        let (repo, id, tx, repaint) = (repo.clone(), pr.id, self.tx.clone(), self.repaint.clone());
        let head = head.to_string();
        let spawned = std::thread::Builder::new()
            .name("review-action".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    // never approve code other than what is on screen
                    match poster.current_head(&repo, id) {
                        Err(e) => return Outcome::Unchecked(e.message()),
                        Ok(now) if !same_commit(&now, &head) => return Outcome::Moved,
                        Ok(_) => {}
                    }
                    let result = poster.set_review(&repo, id, a, body.as_deref());
                    // the PR as the server sees it now: reviewers, and did it move meanwhile?
                    // Also after a lost answer, which may or may not have applied.
                    let after = match &result {
                        Ok(()) | Err(PostError::Unconfirmed | PostError::PostedUnreadable) => {
                            poster.current_pr(&repo, id).ok().map(Box::new)
                        }
                        Err(_) => None,
                    };
                    Outcome::Sent { result, after }
                }))
                .unwrap_or_else(|_| Outcome::Sent {
                    result: Err(PostError::Rejected("internal error".into())),
                    after: None,
                });
                let _ = tx.send(Done { action: a, outcome });
                if let Some(c) = repaint {
                    c.request_repaint();
                }
            });
        if spawned.is_err() {
            pr.reviewers = before;
            self.in_flight = None;
            self.error = Some("Could not start a background thread".into());
            return false;
        }
        true
    }

    /// Applies a finished action; true when the server reports a head other than `head`
    /// (the screen is stale).
    pub fn poll(&mut self, pr: &mut PullRequest, head: &str) -> bool {
        let mut stale = false;
        while let Ok(d) = self.rx.try_recv() {
            let Some((a, before)) = self.in_flight.take() else {
                continue;
            };
            debug_assert_eq!(a, d.action);
            match d.outcome {
                Outcome::Moved => {
                    pr.reviewers = before;
                    self.error = Some(STALE_ACTIONS.into());
                    stale = true;
                }
                Outcome::Unchecked(e) => {
                    pr.reviewers = before;
                    self.error = Some(format!("{UNCHECKED}: {e}."));
                }
                Outcome::Sent {
                    result: Ok(()),
                    after,
                } => {
                    self.done = Some(done_text(a).into());
                    if let Some(now) = after {
                        let h = now.source_commit.trim();
                        stale |= !h.is_empty() && !same_commit(h, head);
                        // the server's reviewer list (others may have reviewed meanwhile)
                        pr.reviewers = now.reviewers;
                    }
                    self.updated = Some(pr.reviewers.clone());
                }
                Outcome::Sent {
                    result: Err(PostError::Unconfirmed | PostError::PostedUnreadable),
                    after,
                } => match after {
                    // sent, answer lost: the server's state is adopted, whatever it says
                    Some(now) => {
                        let h = now.source_commit.trim();
                        stale |= !h.is_empty() && !same_commit(h, head);
                        pr.reviewers = now.reviewers;
                        if my_state(pr, self.ctx.me.as_ref()) == Some(state_after(a)) {
                            self.done = Some(done_text(a).into());
                            self.updated = Some(pr.reviewers.clone());
                        } else {
                            self.unconfirmed = Some(UNCONFIRMED_REFRESHED.into());
                        }
                    }
                    None => {
                        pr.reviewers = before;
                        self.unconfirmed = Some(UNCONFIRMED_UNREAD.into());
                    }
                },
                Outcome::Sent { result: Err(e), .. } => {
                    pr.reviewers = before;
                    self.error = Some(format!("Could not {}: {}", verb(a), e.message()));
                }
            }
        }
        stale
    }
}

/// Sets `me`'s state on `pr`, adding me as a participant when not listed.
fn set_state(pr: &mut PullRequest, me: &UserRef, state: ReviewState) {
    match pr.reviewers.iter_mut().find(|r| r.user.id == me.id) {
        Some(r) => r.state = state,
        None => pr.reviewers.push(Reviewer {
            user: me.clone(),
            state,
        }),
    }
}

// ---- Open in Bitbucket ----

/// A workspace or repository slug: non-empty, only `[A-Za-z0-9._-]`, never `.` or `..`.
fn safe_part(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `https://bitbucket.org/<ws>/<repo>/pull-requests/<id>`, or None unless every part is safe.
pub fn bitbucket_url(repo: &RepoRef, id: u64) -> Option<String> {
    (safe_part(&repo.namespace) && safe_part(&repo.slug) && id > 0).then(|| {
        format!(
            "https://bitbucket.org/{}/{}/pull-requests/{id}",
            repo.namespace, repo.slug
        )
    })
}

/// Opens the PR's page in the default browser. The URL is built only from validated parts
/// and handed to the platform's opener as a single argument (no shell involved, see `open`).
pub fn open_in_bitbucket(repo: &RepoRef, id: u64) -> Result<(), String> {
    let url = bitbucket_url(repo, id).ok_or("This repository name cannot be opened safely")?;
    open_url(&url)
}

/// The label of the "open the PR's page" button.
pub fn open_web_label(pr: &PullRequest) -> &'static str {
    match pr.forge {
        prcore::model::Forge::BitbucketCloud => "Open in Bitbucket",
        prcore::model::Forge::GitHub => "Open in GitHub",
        prcore::model::Forge::GitLab => "Open in GitLab",
    }
}

/// The page of a non-Bitbucket PR: the forge's own `web_url`, only when it is https on the
/// repository's host and has nothing a shell or URL handler could misread (spec 10: links
/// from forge responses open on a user click, on the account's host only).
pub fn forge_page_url(repo: &RepoRef, pr: &PullRequest) -> Option<String> {
    let url = pr.web_url.as_str();
    let host = repo.host.to_ascii_lowercase();
    let lower = url.to_ascii_lowercase();
    // plain http only for a repository on a private address (a self-managed forge there)
    let private = prcore::origin::is_private_http_url(&format!("http://{host}/"));
    let prefix = if private && lower.starts_with("http://") {
        format!("http://{host}/")
    } else {
        format!("https://{host}/")
    };
    let clean = url.len() <= 512
        && url
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~/:%".contains(&b));
    (lower.starts_with(&prefix) && clean).then(|| url.to_string())
}

/// Opens the PR's page: Bitbucket's built from the repository, other forges' `web_url`.
pub fn open_pr_page(repo: &RepoRef, pr: &PullRequest) -> Result<(), String> {
    if pr.forge == prcore::model::Forge::BitbucketCloud {
        return open_in_bitbucket(repo, pr.id);
    }
    let url = forge_page_url(repo, pr).ok_or("This pull request's page cannot be opened safely")?;
    open_url(&url)
}

/// Opens a fixed or validated https URL in the default browser (never a shell, see `open`).
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    crate::open::open(url).map_err(|_| "Could not open the browser".to_string())
}

// ---- painting ----

/// What a click in the action bar or its confirmation asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionAct {
    Request(ReviewAction),
    Confirm,
    Cancel,
    DismissNote,
}

/// Approve and Request changes (or their clear forms), for a right-to-left layout.
pub fn action_buttons(
    ui: &mut egui::Ui,
    s: &ReviewActions,
    pr: &PullRequest,
    stale: bool,
    acts: &mut Vec<ActionAct>,
) {
    if s.read_only {
        // Approve and Request changes are not offered on a read-only forge
        crate::review::composer::read_only_badge(ui);
        return;
    }
    let b = s.buttons(pr);
    let why = s.disabled_reason(stale);
    if s.running() {
        ui.spinner();
    }
    // right to left: Request changes is drawn first so Approve sits left of it; an action
    // the forge does not have (clearing a review on GitHub) is not offered
    let caps = providers::provider::capabilities_for(pr.forge);
    for a in [b.request, b.approve]
        .into_iter()
        .filter(|a| caps.allows(*a))
    {
        let resp = ui.add_enabled(why.is_none(), egui::Button::new(label(a)));
        let resp = match why {
            Some(w) => resp.on_disabled_hover_text(w),
            None => resp,
        };
        if resp.clicked() {
            acts.push(ActionAct::Request(a));
        }
    }
}

/// The line under the top bar: stale explanation, unknown identity, result or error.
pub fn action_notes(ui: &mut egui::Ui, s: &ReviewActions, stale: bool, acts: &mut Vec<ActionAct>) {
    if stale {
        ui.colored_label(AMBER, STALE_ACTIONS);
    }
    if s.ctx.me.is_none() {
        ui.label(RichText::new(UNKNOWN_ME).color(GREY).size(12.0));
    }
    if let Some(e) = &s.error {
        ui.horizontal(|ui| {
            ui.colored_label(Color32::from_rgb(235, 87, 87), e);
            if ui.small_button("Dismiss").clicked() {
                acts.push(ActionAct::DismissNote);
            }
        });
    } else if let Some(u) = &s.unconfirmed {
        ui.horizontal(|ui| {
            ui.colored_label(AMBER, u);
            if ui.small_button("Dismiss").clicked() {
                acts.push(ActionAct::DismissNote);
            }
        });
    } else if let Some(d) = &s.done {
        ui.horizontal(|ui| {
            ui.colored_label(Color32::from_rgb(98, 190, 120), d);
            if ui.small_button("Dismiss").clicked() {
                acts.push(ActionAct::DismissNote);
            }
        });
    }
}

/// The confirmation: question, PR title and #id, amber warnings, the review text where the
/// forge requires one, then the buttons.
pub fn action_confirm(
    ctx: &egui::Context,
    s: &mut ReviewActions,
    pr: &PullRequest,
    acts: &mut Vec<ActionAct>,
) {
    let Some(c) = &mut s.confirm else { return };
    let needs_body = body_required(pr.forge, c.action);
    let resp = egui::Modal::new(Id::unique("review-action-confirm")).show(ctx, |ui| {
        ui.set_min_width(440.0);
        ui.set_max_width(600.0);
        ui.heading(question(c.action));
        ui.add_space(4.0);
        ui.label(RichText::new(format!("#{} {}", pr.id, pr.title)).strong());
        if !c.warnings.is_empty() {
            ui.add_space(6.0);
            for w in &c.warnings {
                ui.label(RichText::new(format!("\u{2022} {w}")).color(AMBER));
            }
        }
        if let Some(n) = &c.note {
            ui.add_space(4.0);
            ui.label(RichText::new(n).color(GREY));
        }
        if needs_body {
            ui.add_space(6.0);
            ui.label("GitHub needs a comment to request changes:");
            ui.add(
                egui::TextEdit::multiline(&mut c.body)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
        }
        let ready = !needs_body || !c.body.trim().is_empty();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ready, egui::Button::new(label(c.action)))
                .clicked()
            {
                acts.push(ActionAct::Confirm);
            }
            if ui.button("Cancel").clicked() {
                acts.push(ActionAct::Cancel);
            }
        });
    });
    if resp.should_close() && !acts.contains(&ActionAct::Confirm) {
        acts.push(ActionAct::Cancel);
    }
}

#[cfg(test)]
mod tests;
