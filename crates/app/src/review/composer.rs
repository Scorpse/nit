//! Comment drafts: the composer, local-first storage, the confirmed sequential submit,
//! replies and resolve / unresolve.
//!
//! Drafts live in the `Store` until the server accepted them (2xx): a draft is deleted only
//! after its post succeeded, so a failed post or a restart never loses one. A draft is
//! marked in the store right before it is sent; when the answer is lost after sending
//! (timeout, reset, 5xx) or the app stops meanwhile, the draft is "unconfirmed" and never
//! submitted again until the user checked the comments and chose "Post again anyway".
//! Inline drafts are re-checked right before posting against the
//! server's current source commit and the file's diff rows (`inline_allowed`); a blocked
//! one is never posted and never silently turned into another kind of comment.
//!
//! `Composer` is the testable state; the `*_card` / `drafts_bar` / `confirm_dialog` /
//! `toast` functions only paint it and push `Act`s that `ReviewScreen::act` applies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, vec2, Align2, Color32, FontId, Id, Key, Modifiers, Painter, Pos2, Rect, RichText, Sense,
    Stroke, StrokeKind, Ui,
};
use prcore::comment_type::CommentType;
use prcore::comments::{
    inline_allowed, AnchorModel, Comment, CommentId, InlineBlock, NewComment, PostRevision,
    ReviewAction, ServerRevision, Thread, ThreadId,
};
use prcore::commit::same_commit as same_commit_strict;
use prcore::diff::{Row, RowKind};
use prcore::draft::{Draft, DraftKind, Side};
use prcore::model::{Forge, PullRequest, RepoRef, UserRef};
use providers::provider::{capabilities_for, SuggestionSyntax};
use store::Store;

use crate::lock::lock_ok;

use super::comments_ui::{
    body_galley, body_width, Comments, Placement, AUTHOR_H, CARD_MARGIN, CARD_PAD, COMMENT_GAP,
    TITLE_H,
};
pub use super::memory_poster::MemoryPoster;

/// Flip to true (here only) once the owner's anchoring experiment showed that inline
/// comments land on the intended line in Bitbucket. While false, inline drafts cannot be
/// posted (they stay saved); `NIT_INLINE_EXPERIMENT=1` lifts that for the experiment only.
pub const INLINE_VERIFIED: bool = false;

/// Environment variable that enables inline posting for the anchoring experiment.
pub const INLINE_EXPERIMENT_VAR: &str = "NIT_INLINE_EXPERIMENT";

/// Whether inline drafts may be posted: verified, or the experiment flag is set to `1`.
/// Read once per process.
pub fn inline_posting_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        INLINE_VERIFIED || std::env::var(INLINE_EXPERIMENT_VAR).is_ok_and(|v| v == "1")
    })
}

/// Whether inline drafts may be posted on `forge`. GitHub and GitLab have their own
/// compile-time gates (`INLINE_ENABLED_GITHUB`, `INLINE_ENABLED_GITLAB`, flipped only by the
/// owner after each forge's anchoring findings); the Bitbucket experiment flag lifts neither.
pub fn inline_posting_enabled_for(forge: Forge) -> bool {
    match forge {
        Forge::GitHub => providers::github::INLINE_ENABLED_GITHUB,
        Forge::GitLab => providers::gitlab::INLINE_ENABLED_GITLAB,
        Forge::BitbucketCloud => inline_posting_enabled(),
    }
}

/// The refusal shown on Submit for `n` inline drafts while inline posting is disabled.
fn inline_disabled_message(n: usize) -> String {
    format!(
        "{n} inline draft(s) not posted: inline comments are disabled until their placement \
         is verified. They stay saved. Use \"Post as file comment instead\" or \"Post as PR \
         comment instead\" on a draft to post it another way."
    )
}

/// Shown next to every inline draft in the submit dialog while `INLINE_VERIFIED` is false.
pub const INLINE_UNVERIFIED_NOTE: &str =
    "Inline placement is not verified yet: check the line on Bitbucket after posting.";

/// How long the resolve / unresolve toast offers Undo.
pub const UNDO_FOR: Duration = Duration::from_secs(5);

/// Characters of a draft shown in lists and the submit dialog.
pub const PREVIEW_CHARS: usize = 80;

/// Why a post did not go through; text is a category, never URLs, bodies or secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostError {
    /// The server refused (auth, access, not found, rate limit, other status).
    Rejected(String),
    /// The server accepted the comment but its answer could not be read.
    PostedUnreadable,
    /// Not sent (the connection could not be made): definitely not posted.
    Network(String),
    Invalid(String),
    /// Sent, but no answer arrived (timeout, reset, 5xx): it may have been posted.
    Unconfirmed,
}

impl PostError {
    pub fn message(&self) -> String {
        match self {
            PostError::Rejected(c) => format!("rejected by the server ({c})"),
            PostError::PostedUnreadable => "posted, response unreadable".into(),
            PostError::Unconfirmed => "the connection failed after sending".into(),
            PostError::Network(c) => format!("network problem ({c})"),
            PostError::Invalid(c) => format!("invalid comment ({c})"),
        }
    }
}

/// Posts comments, (un)resolves threads and sets my review; the real one runs `Provider`
/// calls on the backend runtime, the demo one keeps everything in memory.
pub trait CommentPoster {
    /// Posts `c` on `at`, the revision the review was fetched at (spec 4.3): the provider
    /// sends exactly it and never substitutes the latest.
    fn post(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, PostError>;
    fn resolve(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), PostError>;
    /// The PR's source commit as the server sees it right now (`Provider::pr`).
    fn current_head(&self, repo: &RepoRef, pr_id: u64) -> Result<String, PostError>;
    /// How the forge pins an inline comment (`Capabilities::inline_anchor`).
    fn anchor_model(&self, _repo: &RepoRef) -> AnchorModel {
        AnchorModel::CurrentDiff
    }
    /// The PR's revision as the server sees it right now (`Provider::revision`); by default
    /// only the head is known.
    fn current_revision(&self, repo: &RepoRef, pr_id: u64) -> Result<ServerRevision, PostError> {
        self.current_head(repo, pr_id).map(|head| ServerRevision {
            head,
            ..ServerRevision::default()
        })
    }
    /// The PR as the server sees it right now (`Provider::pr`): head and reviewers.
    fn current_pr(&self, repo: &RepoRef, pr_id: u64) -> Result<PullRequest, PostError>;
    /// Approve / request changes or clear either (`Provider::set_review`), one call. `body`:
    /// the review text where the forge takes one (GitHub requires it to request changes).
    fn set_review(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), PostError>;
}

pub type Poster = Arc<dyn CommentPoster + Send + Sync>;

/// One file of the open review as the inline check needs it.
pub struct FileRows {
    /// The PR the rows belong to (a batch refuses rows of another PR).
    pub repo: RepoRef,
    pub pr_id: u64,
    /// The head the review was opened (fetched) at.
    pub head: String,
    /// The merge base of that fetch (the diff's base); empty when unknown.
    pub merge_base: String,
    pub rows: Arc<Vec<Row>>,
    pub binary: bool,
}

/// Rows of a file of the open review; builds the patch when it is not cached. Runs on the
/// submit worker thread, never on the UI thread.
pub type RowSource = Arc<dyn Fn(&str) -> Result<FileRows, String> + Send + Sync>;

/// Draft ids being posted right now, shared by every composer of the process (screens come
/// and go while a batch runs): a draft in here is shown as posting and never posted twice.
pub type InFlight = Arc<Mutex<HashSet<i64>>>;

/// What a composer is writing: the anchor of the draft it saves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pr,
    File(String),
    /// `line` on `side`; a multi-line comment also has its first line and side in `start`
    /// (`line` is then its last line).
    Inline {
        path: String,
        side: Side,
        line: u32,
        start: Option<(Side, u32)>,
    },
    Reply {
        thread: ThreadId,
    },
}

impl Target {
    pub fn of(d: &Draft) -> Option<Target> {
        Some(match d.kind {
            DraftKind::Pr => Target::Pr,
            DraftKind::File => Target::File(d.path.clone()?),
            DraftKind::Inline => Target::Inline {
                path: d.path.clone()?,
                side: d.side?,
                line: d.line?,
                start: d.start,
            },
            DraftKind::Reply => Target::Reply {
                thread: d.reply_to.clone()?,
            },
        })
    }

    fn fill(&self, d: &mut Draft) {
        (d.path, d.line, d.side, d.start, d.reply_to) = (None, None, None, None, None);
        d.kind = match self {
            Target::Pr => DraftKind::Pr,
            Target::File(p) => {
                d.path = Some(p.clone());
                DraftKind::File
            }
            Target::Inline {
                path,
                side,
                line,
                start,
            } => {
                (d.path, d.side, d.line) = (Some(path.clone()), Some(*side), Some(*line));
                d.start = *start;
                DraftKind::Inline
            }
            Target::Reply { thread } => {
                d.reply_to = Some(thread.clone());
                DraftKind::Reply
            }
        };
    }
}

/// The comment a draft becomes; None for a draft missing its anchor fields. Its text is
/// `Draft::posted_text`: the type's prefix is added here, from the type, on every attempt.
fn new_comment(d: &Draft) -> Option<NewComment> {
    let text = d.posted_text();
    Some(match Target::of(d)? {
        Target::Pr => NewComment::Pr { text },
        Target::File(path) => NewComment::File { path, text },
        Target::Inline {
            path,
            side,
            line,
            start,
        } => NewComment::Inline {
            path,
            side,
            line,
            start,
            text,
        },
        Target::Reply { thread } => NewComment::Reply { thread, text },
    })
}

fn side_word(s: Side) -> &'static str {
    match s {
        Side::New => "new",
        Side::Old => "old",
    }
}

/// "src/a.rs:10-12 (new side)" or "src/a.rs:9 (old side) to 12 (new side)" for a range,
/// "12 (new side)" for one line.
fn lines_word(side: Side, line: u32, start: Option<(Side, u32)>) -> String {
    match start {
        None => format!("{line} ({} side)", side_word(side)),
        Some((s, n)) if s == side => format!("{n}-{line} ({} side)", side_word(side)),
        Some((s, n)) => format!(
            "{n} ({} side) to {line} ({} side)",
            side_word(s),
            side_word(side)
        ),
    }
}

/// "src/a.rs:10 (new side)", "src/a.rs (file)", "Pull request", "Reply to thread #4".
pub fn describe(d: &Draft) -> String {
    match Target::of(d) {
        Some(Target::Pr) => "Pull request".into(),
        Some(Target::File(p)) => format!("{p} (file)"),
        Some(Target::Inline {
            path,
            side,
            line,
            start,
        }) => {
            format!("{path}:{}", lines_word(side, line, start))
        }
        Some(Target::Reply { thread }) => format!("Reply to thread #{thread}"),
        None => "Incomplete draft".into(),
    }
}

pub fn kind_label(k: DraftKind) -> &'static str {
    match k {
        DraftKind::Pr => "PR",
        DraftKind::File => "File",
        DraftKind::Inline => "Inline",
        DraftKind::Reply => "Reply",
    }
}

/// The first `PREVIEW_CHARS` characters on one line.
pub fn preview(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one.chars().take(PREVIEW_CHARS).collect();
    if one.chars().count() > PREVIEW_CHARS {
        out.push_str("...");
    }
    out
}

pub fn block_text(b: InlineBlock, line: Option<u32>) -> String {
    match b {
        InlineBlock::HeadMismatch => {
            "the pull request changed since it was loaded; reload to comment on lines".into()
        }
        InlineBlock::BaseMismatch => {
            "the pull request's base changed since it was loaded; reload to comment on lines".into()
        }
        InlineBlock::BinaryFile => "binary files have no lines to comment on".into(),
        InlineBlock::LineNotInDiff => match line {
            Some(n) => format!("line {n} is not in this diff"),
            None => "the line is not in this diff".into(),
        },
    }
}

/// A comment text as compared by "Check comments" (spec 7.5), the same way on both sides:
/// GitLab strips every carriage return and the trailing whitespace of each line before
/// saving; elsewhere line endings are unified and trailing blanks of every line and of the
/// whole text dropped (the server may normalise those).
pub fn normalize_for_match(forge: Forge, s: &str) -> String {
    let unified = match forge {
        Forge::GitLab => s.replace('\r', ""),
        Forge::BitbucketCloud | Forge::GitHub => s.replace("\r\n", "\n"),
    };
    unified
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Why a GitLab draft is refused (decision D7).
pub const QUICK_ACTION: &str = "On GitLab a line starting with \"/\" and a letter would run a \
     quick action (it can change the merge request). Reword that line.";

/// Decision D7: on GitLab, text with a line starting with `/` and a letter (leading blanks
/// ignored, to be safe) is refused, so a comment can never change the merge request by
/// accident. Other forges take any text.
pub fn quick_action_guard(forge: Forge, text: &str) -> Result<(), String> {
    if forge != Forge::GitLab {
        return Ok(());
    }
    let runs = text.lines().any(|l| {
        let mut ch = l.trim_start_matches([' ', '\t']).chars();
        ch.next() == Some('/') && ch.next().is_some_and(|c| c.is_ascii_alphabetic())
    });
    if runs {
        Err(QUICK_ACTION.into())
    } else {
        Ok(())
    }
}

/// `lines` (new-side text of `range`, first to last line) as a suggested change in the
/// forge's syntax. GitHub: a ```` ```suggestion ```` block, replacing the commented lines.
/// GitLab: ```` ```suggestion:-N+M ````, N lines above and M below `anchor_line` (the
/// commented line).
pub fn suggestion_block(
    syntax: SuggestionSyntax,
    lines: &[String],
    anchor_line: u32,
    range: (u32, u32),
) -> String {
    let mut out = match syntax {
        SuggestionSyntax::GitHubFence => "```suggestion\n".to_string(),
        SuggestionSyntax::GitLabRange => format!(
            "```suggestion:-{}+{}\n",
            anchor_line.saturating_sub(range.0),
            range.1.saturating_sub(anchor_line)
        ),
    };
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("```");
    out
}

/// GitLab takes at most this many lines above and below the commented line.
const SUGGESTION_MAX_OFFSET: u32 = 100;

/// The comment sits where the draft would have been posted.
fn same_place(c: &Comment, d: &Draft) -> bool {
    let a = c.anchor.as_ref();
    match Target::of(d) {
        Some(Target::Pr) => c.parent_id.is_none() && a.is_none(),
        Some(Target::File(p)) => {
            c.parent_id.is_none()
                && a.is_some_and(|a| a.path == p && a.from.is_none() && a.to.is_none())
        }
        Some(Target::Inline {
            path,
            side,
            line,
            start,
        }) => {
            // a range also needs its first line (its side is reported differently per forge)
            c.parent_id.is_none()
                && a.is_some_and(|a| {
                    a.path == path
                        && match side {
                            Side::New => a.to == Some(line),
                            Side::Old => a.from == Some(line) && a.to.is_none(),
                        }
                        && (start.is_none() || a.start_line() == start.map(|s| s.1))
                })
        }
        Some(Target::Reply { thread }) => {
            // Bitbucket: the reply's parent is the thread's root comment. Forges that group
            // by thread: a parentless comment of that thread other than its root.
            c.parent_id.as_ref().map(ThreadId::from).as_ref() == Some(&thread)
                || (c.parent_id.is_none()
                    && c.thread.as_ref() == Some(&thread)
                    && c.id.as_str() != thread.as_str())
        }
        None => false,
    }
}

/// The server head to compare in `inline_allowed`: the local head when both name the same
/// commit (Bitbucket returns abbreviated hashes), otherwise the server's value unchanged.
fn head_for_check(server: &str, local: &str) -> String {
    if same_commit_strict(server, local) {
        local.to_string()
    } else {
        server.to_string()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DraftState {
    #[default]
    Idle,
    /// Being posted right now: locked.
    Posting,
    /// Not posted: the inline check refused it.
    Blocked(InlineBlock),
    /// Not posted: written on an older version of the PR, its line may mean other code now.
    Outdated,
    /// Not posted: the post failed with this message.
    Failed(String),
    /// Posted, but the local copy could not be deleted; never submitted again.
    PostedNotRemoved,
    /// Sent without a known outcome (the answer was lost, or the app stopped meanwhile):
    /// never submitted again until "Check comments" (`checked`) and then "Post again
    /// anyway". `looks_posted`: the check found my comment with the same text in its place.
    Unconfirmed { checked: bool, looks_posted: bool },
}

/// The note of a draft whose post has no known outcome.
pub const UNCONFIRMED_TEXT: &str = "Not confirmed: the connection failed after sending. \
     Refresh comments to check before posting again.";

/// Added to `UNCONFIRMED_TEXT` when the check found an identical comment of mine.
pub const LOOKS_POSTED: &str = "It looks like this comment was posted.";

/// A resolve's answer was lost; the comments are being fetched again.
pub const RESOLVE_REFRESHING: &str =
    "Not confirmed: the connection failed after sending. Refreshing the comments to check.";

/// A resolve's answer was lost and the refreshed comments do not show it applied.
pub const RESOLVE_REFRESHED: &str = "Not confirmed: the connection failed after sending. \
     The comments were refreshed and show the thread as it is on Bitbucket now.";

/// Shown when "Check comments" found no comment of mine with this text in its place.
pub const NOT_FOUND: &str = "Checked: no comment of yours with this text is in its place, \
     so it was probably not posted. Not confirmed: the connection failed after sending.";

impl DraftState {
    /// The line under a draft card's title, if any.
    pub fn note(&self, d: &Draft) -> Option<(String, Color32)> {
        match self {
            DraftState::Unconfirmed {
                checked,
                looks_posted,
            } => Some((
                match (checked, looks_posted) {
                    (_, true) => format!("{LOOKS_POSTED} {UNCONFIRMED_TEXT}"),
                    (true, false) => NOT_FOUND.into(),
                    (false, false) => UNCONFIRMED_TEXT.into(),
                },
                AMBER,
            )),
            DraftState::Blocked(b) => Some((format!("blocked: {}", block_text(*b, d.line)), AMBER)),
            DraftState::Failed(e) => Some((format!("Not posted: {e}"), RED)),
            DraftState::PostedNotRemoved => {
                Some(("Posted, cleaning up the local copy.".into(), AMBER))
            }
            DraftState::Outdated => Some((
                "blocked: written on an older version of the pull request".into(),
                AMBER,
            )),
            DraftState::Idle | DraftState::Posting => None,
        }
    }
}

/// The open composer (at most one).
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
enum Event {
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
    repo: RepoRef,
    pr_id: u64,
    store: Arc<Mutex<Store>>,
    /// The fetched head (`Opened`), stored with new drafts.
    pub head: String,
    /// The PR moved since it was fetched: inline drafts are held.
    pub stale: bool,
    /// Inline drafts may be posted (see `inline_posting_enabled`); else they stay saved.
    inline_enabled: bool,
    poster: Option<Poster>,
    rows: Option<RowSource>,
    ctx: Option<egui::Context>,
    /// Drafts of this PR, oldest first.
    pub drafts: Vec<Draft>,
    states: HashMap<i64, DraftState>,
    pub editor: Option<Editor>,
    /// Some while the submit confirmation is open: the drafts it lists.
    pub confirm: Option<Vec<i64>>,
    batch: bool,
    /// Ids of the running batch (for "Posting 2 of 5").
    batch_ids: Vec<i64>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// Resolves waiting for the server: root id -> the state asked for.
    resolving: HashMap<CommentId, bool>,
    pub toast: Option<Toast>,
    pub error: Option<String>,
    /// A resolve whose answer was lost: not a failure, the comments are refetched (amber).
    pub notice: Option<String>,
    /// Resolves with a lost answer: root id -> the state asked for, until comments arrive.
    unconfirmed_resolves: HashMap<CommentId, bool>,
    /// Bumped on every change that can alter a draft card or the composer's height.
    ver: u64,
    /// Computed in the layout pass, read when painting the rows.
    plan: std::cell::RefCell<RowPlan>,
    in_flight: InFlight,
    /// Set to stop the running batch after its current draft (Back, Reload, drop).
    cancel: Arc<AtomicBool>,
    /// "Stop after this comment" was clicked for the running batch.
    stopping: bool,
    /// Drafts the running batch has posted (for "Posting N of M").
    batch_done: usize,
    /// Unconfirmed drafts whose "Check comments" waits for the refreshed comments.
    checking: HashSet<i64>,
    /// Unconfirmed drafts: the text that was sent (the draft may be edited afterwards).
    sent_text: HashMap<i64, String>,
    /// Test hook: this many store deletes fail.
    #[cfg(test)]
    fail_deletes: Arc<AtomicUsize>,
    /// Test hook: this many tombstone writes fail.
    #[cfg(test)]
    fail_marks: Arc<AtomicUsize>,
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
    fn with_failing_store(
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
    fn with_failing_deletes(
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
    fn load(&mut self) {
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
    fn refuse_in_flight(&mut self, id: i64) -> bool {
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

    fn is_in_flight(&self, id: i64) -> bool {
        lock_ok(&self.in_flight).contains(&id)
    }

    fn delete_stored(&self, id: i64) -> bool {
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        lock_ok(&self.store)
    }

    fn bump(&mut self) {
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

    // ---- editing ----

    /// Opens an empty composer for `target`; a composer with unsaved text is kept as a draft.
    pub fn open_editor(&mut self, target: Target) {
        if self.read_only {
            self.error = Some(format!("Commenting is {READ_ONLY_TIP}"));
            return;
        }
        if self.batch {
            return;
        }
        if matches!(target, Target::Inline { start: Some(_), .. }) && !self.multi_line() {
            self.error = Some("Comments on a range of lines are not supported here".into());
            return;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.target == target && e.draft_id.is_none())
        {
            return;
        }
        self.stash_editor();
        self.editor = Some(Editor {
            target,
            text: String::new(),
            draft_id: None,
            comment_type: None,
            focus: true,
            suggestions: capabilities_for(self.forge).suggestions,
        });
        self.bump();
    }

    /// The forge takes comments on a range of lines.
    pub fn multi_line(&self) -> bool {
        capabilities_for(self.forge).multi_line_comments
    }

    /// "Suggest change": appends the composer's new-side lines (from `rows`, the file's diff
    /// rows) as a suggestion in the forge's syntax. Refused, with a message, for old-side
    /// lines and for lines not shown in `rows`.
    pub fn insert_suggestion(&mut self, rows: &[Row]) {
        let Some(e) = self.editor.as_mut() else {
            return;
        };
        let Some(syntax) = e.suggestions else {
            return;
        };
        let Target::Inline {
            side: Side::New,
            line,
            start,
            ..
        } = e.target
        else {
            self.error = Some("Suggestions apply to new-side lines only".into());
            return;
        };
        let first = match start {
            None => line,
            Some((Side::New, n)) => n,
            Some((Side::Old, _)) => {
                self.error = Some("Suggestions apply to new-side lines only".into());
                return;
            }
        };
        if syntax == SuggestionSyntax::GitLabRange && line - first > SUGGESTION_MAX_OFFSET {
            self.error = Some(format!(
                "GitLab takes suggestions of at most {SUGGESTION_MAX_OFFSET} lines above the \
                 commented line"
            ));
            return;
        }
        let lines: Vec<String> = rows
            .iter()
            .filter(|r| r.new_line.is_some_and(|n| (first..=line).contains(&n)))
            .map(|r| r.text.clone())
            .collect();
        if lines.len() as u64 != u64::from(line - first) + 1 {
            self.error = Some("The commented lines are not all shown in this diff".into());
            return;
        }
        if !e.text.is_empty() && !e.text.ends_with('\n') {
            e.text.push('\n');
        }
        e.text
            .push_str(&suggestion_block(syntax, &lines, line, (first, line)));
        e.focus = true;
        self.bump();
    }

    /// Opens the composer on an existing draft.
    pub fn edit_draft(&mut self, id: i64) {
        if self.batch || self.editor.as_ref().is_some_and(|e| e.draft_id == Some(id)) {
            return;
        }
        if self.refuse_in_flight(id) {
            return;
        }
        let Some(d) = self.draft(id) else { return };
        let Some(target) = Target::of(d) else { return };
        let (text, comment_type) = (d.text.clone(), d.comment_type);
        self.stash_editor();
        self.editor = Some(Editor {
            target,
            text,
            draft_id: Some(id),
            comment_type,
            focus: true,
            suggestions: capabilities_for(self.forge).suggestions,
        });
        self.bump();
    }

    fn stash_editor(&mut self) {
        if let Some(e) = self.editor.take() {
            if !e.text.trim().is_empty() {
                self.save(e);
            }
        }
    }

    /// Ctrl+Enter / "Save draft": stores the composer's text. Empty text is never saved;
    /// text that would run a GitLab quick action is refused and stays in the composer (a
    /// draft stashed by opening another composer is refused when posting instead).
    pub fn save_editor(&mut self) -> bool {
        let Some(e) = self.editor.take() else {
            return false;
        };
        self.bump();
        if e.text.trim().is_empty() {
            return false;
        }
        if let Err(why) = quick_action_guard(self.forge, &e.text) {
            self.error = Some(why);
            self.editor = Some(e);
            return false;
        }
        self.save(e)
    }

    pub fn cancel_editor(&mut self) {
        if self.editor.take().is_some() {
            self.bump();
        }
    }

    fn save(&mut self, e: Editor) -> bool {
        let existing = e.draft_id.and_then(|id| self.draft(id)).cloned();
        let mut d = existing.unwrap_or_else(|| Draft {
            id: 0,
            repo: self.repo.clone(),
            pr_id: self.pr_id,
            kind: DraftKind::Pr,
            path: None,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text: String::new(),
            source_commit: self.head.clone(),
            comment_type: None,
        });
        e.target.fill(&mut d);
        d.text = e.text.clone();
        d.comment_type = e.comment_type;
        let saved = self.lock().save_draft(&d);
        self.bump();
        match saved {
            Ok(id) => {
                d.id = id;
                self.drafts.retain(|x| x.id != id);
                self.drafts.push(d);
                self.drafts.sort_by_key(|x| x.id);
                // an edited draft is checked again on the next submit; one that may already
                // be posted stays unconfirmed (only "Post again anyway" re-arms it)
                if !matches!(self.states.get(&id), Some(DraftState::Unconfirmed { .. })) {
                    self.states.remove(&id);
                }
                true
            }
            Err(_) => {
                self.error = Some("Could not save the draft".into());
                self.editor = Some(e);
                false
            }
        }
    }

    /// Deletes a draft locally (it was never posted).
    pub fn delete_draft(&mut self, id: i64) {
        if self.batch || self.refuse_in_flight(id) {
            return;
        }
        if self.lock().delete_draft(id).is_err() {
            self.error = Some("Could not delete the draft".into());
            return;
        }
        self.drafts.retain(|d| d.id != id);
        self.states.remove(&id);
        if self.editor.as_ref().is_some_and(|e| e.draft_id == Some(id)) {
            self.editor = None;
        }
        self.bump();
    }

    /// "Post as file comment instead" on an inline draft: an explicit click that replaces it
    /// by a new file draft whose text starts with `(line N)`. Nothing is posted here.
    pub fn as_file_comment(&mut self, id: i64) {
        self.convert_inline(id, DraftKind::File);
    }

    /// "Post as PR comment instead" on an inline draft: an explicit click that replaces it
    /// by a new PR draft whose text starts with `(path line N)`. Nothing is posted here.
    pub fn as_pr_comment(&mut self, id: i64) {
        self.convert_inline(id, DraftKind::Pr);
    }

    fn convert_inline(&mut self, id: i64, to: DraftKind) {
        if self.batch || self.refuse_in_flight(id) {
            return;
        }
        let Some(d) = self.draft(id).cloned() else {
            return;
        };
        let (DraftKind::Inline, Some(path)) = (d.kind, d.path.clone()) else {
            return;
        };
        // a range keeps its first line in the text: "(lines 9-12)"
        let lines = |n: u32| match d.start {
            Some((_, s)) => format!("lines {s}-{n}"),
            None => format!("line {n}"),
        };
        let (kept_path, text) = match (to, d.line) {
            (DraftKind::File, Some(n)) => (Some(path), format!("({}) {}", lines(n), d.text)),
            (DraftKind::File, None) => (Some(path), d.text.clone()),
            (_, Some(n)) => (None, format!("({path} {}) {}", lines(n), d.text)),
            (_, None) => (None, format!("({path}) {}", d.text)),
        };
        let file = Draft {
            id: 0,
            kind: to,
            path: kept_path,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text,
            ..d
        };
        let saved = self.lock().save_draft(&file);
        match saved {
            Ok(new_id) => {
                self.drafts.push(Draft { id: new_id, ..file });
                self.drafts.sort_by_key(|x| x.id);
                self.delete_draft(id);
            }
            Err(_) => self.error = Some("Could not save the file draft".into()),
        }
        self.bump();
    }

    /// After a (re)fetch: inline drafts written on another head than `head` are flagged as
    /// outdated at once (a submit checks every inline draft again anyway).
    pub fn revalidate_heads(&mut self) {
        for d in &self.drafts {
            if d.kind == DraftKind::Inline && !same_commit_strict(&d.source_commit, &self.head) {
                let st = self.states.entry(d.id).or_default();
                if *st == DraftState::Idle {
                    *st = DraftState::Outdated;
                }
            }
        }
        self.bump();
    }

    /// Scene setup and tests: marks a draft as blocked without running a batch.
    pub fn mark_blocked(&mut self, id: i64, block: InlineBlock) {
        self.states.insert(id, DraftState::Blocked(block));
        self.bump();
    }

    // ---- unconfirmed posts ----

    /// "Check comments" on an unconfirmed draft: the next comments that arrive are compared
    /// with it. True when the caller should refresh the comments now.
    pub fn check_comments(&mut self, id: i64) -> bool {
        if !matches!(self.state(id), DraftState::Unconfirmed { .. }) {
            return false;
        }
        self.checking.insert(id);
        true
    }

    /// Freshly fetched comments: every draft waiting for a check is marked checked, with a
    /// hint when a comment of mine with exactly its text sits in its place. Nothing is
    /// deleted or posted here.
    pub fn comments_checked(&mut self, comments: &Comments, me: Option<&UserRef>) {
        for id in std::mem::take(&mut self.checking) {
            let Some(d) = self.draft(id) else { continue };
            if !matches!(self.state(id), DraftState::Unconfirmed { .. }) {
                continue;
            }
            // what was sent, and what the draft would send now (edited after the unknown
            // outcome); both with the type's prefix, exactly as posted
            let now = d.posted_text();
            let texts: Vec<String> = self
                .sent_text
                .get(&id)
                .into_iter()
                .chain(std::iter::once(&now))
                .map(|t| normalize_for_match(self.forge, t))
                .collect();
            let looks_posted = me.is_some_and(|me| {
                !me.id.is_empty()
                    && comments
                        .threads
                        .iter()
                        .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
                        .any(|c| {
                            !c.deleted
                                && c.author.id == me.id
                                && texts.contains(&normalize_for_match(self.forge, &c.body))
                                && same_place(c, d)
                        })
            });
            self.states.insert(
                id,
                DraftState::Unconfirmed {
                    checked: true,
                    looks_posted,
                },
            );
            self.bump();
        }
    }

    /// "Post again anyway": an explicit, per-draft choice, offered only after the comments
    /// were checked. The draft becomes postable again. Returns whether it was re-armed.
    pub fn rearm(&mut self, id: i64) -> bool {
        if self.batch
            || !matches!(
                self.state(id),
                DraftState::Unconfirmed { checked: true, .. }
            )
        {
            return false;
        }
        if self.lock().clear_draft_posting(id).is_err() {
            self.error = Some("Could not update the draft".into());
            return false;
        }
        self.states.remove(&id);
        self.sent_text.remove(&id);
        self.bump();
        true
    }

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

    fn resolve_to(
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

    // ---- placement ----

    /// The draft open in the composer (its card is replaced by the composer).
    fn editing(&self) -> Option<i64> {
        self.editor.as_ref().and_then(|e| e.draft_id)
    }

    /// Reply drafts under thread `t`, oldest first (not the one being edited).
    pub fn replies_to<'a>(&'a self, t: &'a Thread) -> impl Iterator<Item = &'a Draft> + 'a {
        let editing = self.editing();
        self.drafts
            .iter()
            .filter(move |d| d.replies_to(t) && Some(d.id) != editing)
    }

    pub fn editor_is(&self, target: &Target) -> bool {
        self.editor.as_ref().is_some_and(|e| &e.target == target)
    }

    pub fn of_kind<'a>(
        &'a self,
        kind: DraftKind,
        path: Option<&'a str>,
    ) -> impl Iterator<Item = &'a Draft> + 'a {
        let editing = self.editing();
        self.drafts.iter().filter(move |d| {
            d.kind == kind && (path.is_none() || d.path.as_deref() == path) && Some(d.id) != editing
        })
    }

    /// Lays drafts and the composer onto `path`'s rows next to the placed threads.
    pub fn plan(
        &self,
        path: &str,
        rows: &[Row],
        placement: &Placement,
        threads: &[Thread],
    ) -> RowPlan {
        let mut plan = RowPlan {
            path: path.to_string(),
            ..Default::default()
        };
        let reply_target = match self.editor.as_ref().map(|e| &e.target) {
            Some(Target::Reply { thread }) => Some(thread),
            _ => None,
        };
        for (row, ids) in &placement.inline {
            let stack = plan.rows.entry(*row).or_default();
            for &t in ids {
                stack.push(Slot::Thread(t));
                let th = &threads[t];
                stack.extend(self.replies_to(th).map(|d| Slot::Draft(d.id)));
                if th.id().is_some() && reply_target == th.id() {
                    stack.push(Slot::Editor);
                }
            }
        }
        let inline: Vec<&Draft> = self.of_kind(DraftKind::Inline, Some(path)).collect();
        let editor_at = match self.editor.as_ref().map(|e| &e.target) {
            Some(Target::Inline {
                path: p,
                side,
                line,
                ..
            }) if p == path => Some((*side == Side::New, *line)),
            _ => None,
        };
        let mut wanted: HashSet<(bool, u32)> = inline
            .iter()
            .filter_map(|d| Some((d.side? == Side::New, d.line?)))
            .collect();
        wanted.extend(editor_at);
        let at = row_index(rows, &wanted);
        for d in inline {
            match d
                .side
                .zip(d.line)
                .and_then(|(s, n)| at.get(&(s == Side::New, n)))
            {
                Some(&r) => plan.rows.entry(r).or_default().push(Slot::Draft(d.id)),
                None => plan.unplaced.push(d.id),
            }
        }
        if let Some(k) = editor_at {
            match at.get(&k) {
                Some(&r) => plan.rows.entry(r).or_default().push(Slot::Editor),
                None => plan.editor_unplaced = true,
            }
        }
        plan
    }

    /// Extra heights (beyond the thread cards) per row for `Comments::layout_with`; keeps
    /// the plan for painting.
    pub fn row_extras(
        &self,
        path: &str,
        rows: &[Row],
        placement: &Placement,
        threads: &[Thread],
        width: f32,
        measure: &mut dyn FnMut(&str, f32) -> f32,
    ) -> Vec<(usize, f32)> {
        let plan = self.plan(path, rows, placement, threads);
        let out = plan
            .rows
            .iter()
            .filter_map(|(&row, slots)| {
                let h: f32 = slots
                    .iter()
                    .map(|s| match *s {
                        Slot::Thread(_) => 0.0,
                        Slot::Draft(id) => self.draft(id).map_or(0.0, |d| {
                            draft_height(d, &self.state(id), width, measure) + CARD_MARGIN
                        }),
                        Slot::Editor => editor_height() + CARD_MARGIN,
                    })
                    .sum();
                (h > 0.0).then_some((row, h))
            })
            .collect();
        *self.plan.borrow_mut() = plan;
        out
    }

    /// The plan of the last layout of `path`.
    pub fn plan_for(&self, path: &str) -> Option<std::cell::Ref<'_, RowPlan>> {
        let p = self.plan.borrow();
        (p.path == path).then_some(p)
    }
}

/// First row showing each wanted (new side?, line); one pass over the rows.
fn row_index(rows: &[Row], wanted: &HashSet<(bool, u32)>) -> HashMap<(bool, u32), usize> {
    let mut at = HashMap::new();
    if wanted.is_empty() {
        return at;
    }
    for (i, r) in rows.iter().enumerate() {
        if matches!(r.kind, RowKind::Context | RowKind::Add) {
            if let Some(n) = r.new_line.filter(|n| wanted.contains(&(true, *n))) {
                at.entry((true, n)).or_insert(i);
            }
        }
        if matches!(r.kind, RowKind::Context | RowKind::Del) {
            if let Some(n) = r.old_line.filter(|n| wanted.contains(&(false, *n))) {
                at.entry((false, n)).or_insert(i);
            }
        }
    }
    at
}

/// The submit worker: posts drafts in order, stops at the first failure.
struct Batch {
    in_flight: InFlight,
    /// Checked between drafts; the draft being posted is always finished (post + delete).
    cancel: Arc<AtomicBool>,
    /// The head the screen was loaded at when the batch started.
    head: String,
    /// Inline drafts are refused here too, whatever the caller listed.
    inline_enabled: bool,
    /// Text that would run a GitLab quick action is refused here (`quick_action_guard`).
    forge: Forge,
    poster: Poster,
    rows: Option<RowSource>,
    store: Arc<Mutex<Store>>,
    repo: RepoRef,
    pr_id: u64,
    tx: Sender<Event>,
    ctx: Option<egui::Context>,
    #[cfg(test)]
    fail_deletes: Arc<AtomicUsize>,
    #[cfg(test)]
    fail_marks: Arc<AtomicUsize>,
}

/// One draft's outcome: keep going or stop the batch.
enum Step {
    Next,
    Stop,
}

impl Batch {
    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }

    fn run(self, drafts: Vec<Draft>) {
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

    fn delete(&self, id: i64) -> bool {
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

    fn mark(&self, id: i64) -> bool {
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

    fn fail(&self, id: i64, error: String) -> Step {
        self.send(Event::Failed { id, error });
        Step::Stop
    }

    fn one(&self, d: &Draft) -> Step {
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
struct Claim {
    reg: InFlight,
    id: i64,
}

impl Claim {
    fn take(reg: &InFlight, id: i64) -> Option<Claim> {
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

// ---- painting ----

use crate::theme::{AMBER, DRAFT, RED};
const QUIET: Color32 = crate::theme::GREY;
const TEXT: Color32 = crate::theme::CARD_TEXT;
const DRAFT_BG: Color32 = Color32::from_rgb(30, 36, 52);

pub const BUTTON_H: f32 = 20.0;
pub const EDITOR_TEXT_H: f32 = 92.0;
pub const EDITOR_ID: &str = "review-draft-editor";

/// Height of the composer card.
pub fn editor_height() -> f32 {
    2.0 * CARD_PAD + TITLE_H + COMMENT_GAP + EDITOR_TEXT_H + COMMENT_GAP + BUTTON_H
}

/// Height of a draft card at `width`.
pub fn draft_height(
    d: &Draft,
    state: &DraftState,
    width: f32,
    measure: &mut dyn FnMut(&str, f32) -> f32,
) -> f32 {
    let note = if state.note(d).is_some() {
        AUTHOR_H
    } else {
        0.0
    };
    2.0 * CARD_PAD + TITLE_H + note + COMMENT_GAP + measure(&d.text, body_width(width, false))
}

/// Where a draft sits, for its title: "new line 10", "file", "reply", "pull request".
fn place_word(d: &Draft) -> String {
    match (d.kind, d.side, d.line) {
        (DraftKind::Inline, Some(s), Some(n)) => format!("{} line {n}", side_word(s)),
        (DraftKind::File, ..) => "file comment".into(),
        (DraftKind::Reply, ..) => "reply".into(),
        (DraftKind::Pr, ..) => "pull request comment".into(),
        _ => "incomplete".into(),
    }
}

/// Adds `widget` in `rect` inside a card without moving `ui`'s cursor. The caller
/// allocated the whole card; `Ui::put` would advance the cursor to the widget's bottom (a
/// title button's bottom is near the card's top), so the next item overlapped the card.
pub fn put_in_card(ui: &mut Ui, rect: Rect, widget: impl egui::Widget) -> egui::Response {
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
    ))
    .add(widget)
}

/// Small buttons right-aligned in the title row of `card`; returns which was clicked.
fn title_buttons(ui: &mut Ui, card: Rect, labels: &[(&str, bool)]) -> Option<usize> {
    let mut right = card.right() - CARD_PAD;
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    let mut clicked = None;
    for (i, (label, enabled)) in labels.iter().enumerate() {
        let w = ui
            .painter()
            .layout_no_wrap(label.to_string(), FontId::proportional(12.0), TEXT)
            .size()
            .x
            + 2.0 * ui.spacing().button_padding.x
            + 6.0;
        let r = Rect::from_min_size(Pos2::new(right - w, cy - BUTTON_H / 2.0), vec2(w, BUTTON_H));
        let text = RichText::new(*label).size(12.0);
        let b = egui::Button::new(if *enabled { text } else { text.color(QUIET) })
            .wrap_mode(egui::TextWrapMode::Extend);
        if put_in_card(ui, r, b.sense_if(*enabled)).clicked() && *enabled {
            clicked = Some(i);
        }
        right = r.left() - 4.0;
    }
    clicked
}

trait SenseIf {
    fn sense_if(self, on: bool) -> Self;
}

impl SenseIf for egui::Button<'_> {
    fn sense_if(self, on: bool) -> Self {
        if on {
            self
        } else {
            self.sense(Sense::hover())
        }
    }
}

/// Paints one draft into `card` (sized by `draft_height`) with Edit / Delete and, for a
/// blocked inline draft, "Post as file comment instead".
pub fn draft_card(
    ui: &mut Ui,
    painter: &Painter,
    card: Rect,
    d: &Draft,
    state: &DraftState,
    c: &Composer,
    acts: &mut Vec<Act>,
) {
    let locked = c.running();
    let p = painter.with_clip_rect(card.intersect(painter.clip_rect()));
    p.rect_filled(card, 4.0, DRAFT_BG);
    p.rect_stroke(card, 4.0, Stroke::new(1.0, DRAFT), StrokeKind::Inside);
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    let badge = p.layout_no_wrap("draft".into(), FontId::proportional(11.0), Color32::WHITE);
    let pill = Rect::from_min_size(
        Pos2::new(card.left() + CARD_PAD, cy - 8.0),
        vec2(badge.size().x + 12.0, 16.0),
    );
    p.rect_filled(pill, 8.0, DRAFT.gamma_multiply(0.7));
    p.galley(pill.center() - badge.size() / 2.0, badge, Color32::WHITE);
    let mut x = pill.right() + 8.0;
    if let Some(t) = d.comment_type {
        x = super::types_ui::paint_badge(&p, Pos2::new(x, cy), t).right() + 8.0;
    }
    let mut piece = |text: &str, color: Color32| {
        let r = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(13.0),
            color,
        );
        x = r.right() + 8.0;
    };
    piece(&place_word(d), QUIET);
    if *state == DraftState::Posting {
        piece("Posting...", AMBER);
    }
    let mut y = card.top() + CARD_PAD + TITLE_H;
    if let Some((note, color)) = state.note(d) {
        p.text(
            Pos2::new(card.left() + CARD_PAD, y + AUTHOR_H / 2.0),
            Align2::LEFT_CENTER,
            note,
            FontId::proportional(12.0),
            color,
        );
        y += AUTHOR_H;
    }
    y += COMMENT_GAP;
    let body = body_galley(&p, &d.text, body_width(card.width(), false), TEXT);
    p.galley(Pos2::new(card.left() + CARD_PAD, y), body, TEXT);

    if !card.intersects(painter.clip_rect()) {
        return;
    }
    let editable = !locked && *state != DraftState::Posting;
    let blocked = matches!(state, DraftState::Blocked(_) | DraftState::Outdated)
        && d.kind == DraftKind::Inline;
    let mut labels = vec![(if blocked { "Discard" } else { "Delete" }, editable)];
    if *state != DraftState::PostedNotRemoved {
        labels.push(("Edit", editable));
    }
    let off = d.kind == DraftKind::Inline && !c.inline_enabled();
    if blocked || off {
        labels.push((AS_FILE, editable));
    }
    if off {
        labels.push((AS_PR, editable));
    }
    if let DraftState::Unconfirmed { checked, .. } = state {
        labels.push((POST_AGAIN, editable && *checked));
        labels.push((CHECK_COMMENTS, editable));
    }
    match title_buttons(ui, card, &labels).map(|i| labels[i].0) {
        Some("Delete" | "Discard") => acts.push(Act::Delete(d.id)),
        Some("Edit") => acts.push(Act::Edit(d.id)),
        Some(AS_FILE) => acts.push(Act::AsFile(d.id)),
        Some(AS_PR) => acts.push(Act::AsPr(d.id)),
        Some(POST_AGAIN) => acts.push(Act::Rearm(d.id)),
        Some(CHECK_COMMENTS) => acts.push(Act::CheckComments(d.id)),
        _ => {}
    }
}

const AS_FILE: &str = "Post as file comment instead";
const AS_PR: &str = "Post as PR comment instead";
const CHECK_COMMENTS: &str = "Check comments";
const POST_AGAIN: &str = "Post again anyway";

fn editor_title(e: &Editor) -> String {
    let what = match &e.target {
        Target::Pr => "Comment on the pull request".to_string(),
        Target::File(p) => format!("Comment on file {p}"),
        Target::Inline {
            side,
            line,
            start: None,
            ..
        } => {
            format!("Comment on {} line {line}", side_word(*side))
        }
        Target::Inline {
            side, line, start, ..
        } => format!("Comment on lines {}", lines_word(*side, *line, *start)),
        Target::Reply { .. } => "Reply".to_string(),
    };
    if e.draft_id.is_some() {
        format!("Edit draft: {what}")
    } else {
        what
    }
}

/// The composer in `card` (sized by `editor_height`): Ctrl+Enter saves, Esc cancels.
pub fn editor_card(
    ui: &mut Ui,
    painter: &Painter,
    card: Rect,
    e: &mut Editor,
    acts: &mut Vec<Act>,
) {
    let p = painter.with_clip_rect(card.intersect(painter.clip_rect()));
    p.rect_filled(card, 4.0, DRAFT_BG);
    p.rect_stroke(card, 4.0, Stroke::new(1.5, DRAFT), StrokeKind::Inside);
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    p.text(
        Pos2::new(card.left() + CARD_PAD, cy),
        Align2::LEFT_CENTER,
        editor_title(e),
        FontId::proportional(13.0),
        Color32::WHITE,
    );
    if !card.intersects(painter.clip_rect()) {
        return;
    }
    let id = Id::unique(EDITOR_ID);
    // handled before the text field sees them (it would insert a newline / drop focus)
    if ui.memory(|m| m.has_focus(id)) {
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            acts.push(Act::Save);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            acts.push(Act::Cancel);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            e.comment_type = CommentType::cycle(e.comment_type, false);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            e.comment_type = CommentType::cycle(e.comment_type, true);
        }
    }
    let chips_w = super::types_ui::chips_width(ui);
    let chips = Rect::from_min_size(
        Pos2::new(card.right() - CARD_PAD - chips_w, cy - BUTTON_H / 2.0),
        vec2(chips_w, BUTTON_H),
    );
    if let Some(t) = super::types_ui::chips(ui, chips, e.comment_type) {
        e.comment_type = t;
        e.focus = true;
    }
    let top = card.top() + CARD_PAD + TITLE_H + COMMENT_GAP;
    let text_rect = Rect::from_min_size(
        Pos2::new(card.left() + CARD_PAD, top),
        vec2(card.width() - 2.0 * CARD_PAD, EDITOR_TEXT_H),
    );
    let edit = egui::TextEdit::multiline(&mut e.text)
        .id(id)
        .desired_width(text_rect.width())
        // Tab cycles the comment type (handled above) instead of leaving the field
        .lock_focus(true)
        .hint_text(format!(
            "Write a comment. {} saves the draft, Esc cancels, Tab sets the type.",
            super::keymap::CMD_ENTER
        ));
    let resp = put_in_card(ui, text_rect, edit);
    if e.focus {
        resp.request_focus();
        e.focus = false;
    }
    let by = text_rect.bottom() + COMMENT_GAP;
    let save = Rect::from_min_size(Pos2::new(card.left() + CARD_PAD, by), vec2(90.0, BUTTON_H));
    let cancel = Rect::from_min_size(Pos2::new(save.right() + 6.0, by), vec2(64.0, BUTTON_H));
    let empty = e.text.trim().is_empty();
    if put_in_card(ui, save, egui::Button::new("Save draft").sense_if(!empty))
        .on_hover_text(super::keymap::CMD_ENTER)
        .clicked()
        && !empty
    {
        acts.push(Act::Save);
    }
    if put_in_card(ui, cancel, egui::Button::new("Cancel"))
        .on_hover_text("Esc")
        .clicked()
    {
        acts.push(Act::Cancel);
    }
    let mut last = cancel;
    if e.suggestions.is_some()
        && matches!(
            e.target,
            Target::Inline {
                side: Side::New,
                ..
            }
        )
    {
        last = Rect::from_min_size(Pos2::new(cancel.right() + 6.0, by), vec2(110.0, BUTTON_H));
        if put_in_card(ui, last, egui::Button::new("Suggest change"))
            .on_hover_text("Insert the commented lines as a suggested change")
            .clicked()
        {
            acts.push(Act::Suggest);
        }
    }
    p.text(
        Pos2::new(last.right() + 10.0, cancel.center().y),
        Align2::LEFT_CENTER,
        "Saved locally; nothing is posted until you submit.",
        FontId::proportional(12.0),
        QUIET,
    );
}

/// A draft card as a widget (sections outside the virtualized rows).
pub fn draft_widget(ui: &mut Ui, c: &Composer, d: &Draft, acts: &mut Vec<Act>) {
    let w = ui.available_width().min(1000.0);
    let state = c.state(d.id);
    let h = {
        let painter = ui.painter().clone();
        let mut m = super::comments_ui::measure_with(&painter);
        draft_height(d, &state, w, &mut m)
    };
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter().clone();
    draft_card(ui, &painter, rect, d, &state, c, acts);
    ui.add_space(CARD_MARGIN);
}

/// The composer as a widget, when it targets `target`.
pub fn editor_widget(ui: &mut Ui, c: &mut Composer, target: &Target, acts: &mut Vec<Act>) {
    let Some(e) = c.editor.as_mut().filter(|e| &e.target == target) else {
        return;
    };
    let w = ui.available_width().min(1000.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, editor_height()), Sense::hover());
    let painter = ui.painter().clone();
    editor_card(ui, &painter, rect, e, acts);
    ui.add_space(CARD_MARGIN);
}

/// Reply drafts and the reply composer of thread `t`, under its card.
pub fn reply_widgets(ui: &mut Ui, c: &mut Composer, t: &Thread, acts: &mut Vec<Act>) {
    let ids: Vec<i64> = c.replies_to(t).map(|d| d.id).collect();
    for id in ids {
        if let Some(d) = c.draft(id).cloned() {
            draft_widget(ui, c, &d, acts);
        }
    }
    if let Some(thread) = t.id() {
        let target = Target::Reply {
            thread: thread.clone(),
        };
        editor_widget(ui, c, &target, acts);
    }
}

/// Reply and Resolve / Unresolve on a thread card's title row.
pub fn thread_buttons(ui: &mut Ui, card: Rect, t: &Thread, c: &Composer, acts: &mut Vec<Act>) {
    if c.read_only {
        // no Resolve / Reply: nothing is written to this forge in this version
        return;
    }
    let busy = c.is_resolving(&t.root.id);
    let resolve = if t.root.resolved {
        "Unresolve"
    } else {
        "Resolve"
    };
    // GitHub says per thread whether I may (un)resolve it
    let labels = [
        (resolve, !busy && c.can_post() && t.root.can_resolve),
        ("Reply", !c.running()),
    ];
    match title_buttons(ui, card, &labels) {
        Some(0) => acts.push(Act::Resolve(t.root.id.clone())),
        Some(_) => {
            if let Some(thread) = t.id() {
                acts.push(Act::Open(Target::Reply {
                    thread: thread.clone(),
                }))
            }
        }
        None => {}
    }
}

/// Why nothing can be written on a read-only forge (the tooltip of its "Read-only" badge).
/// Forge-neutral: which forges are read-only is `Capabilities::read_only`'s call.
pub const READ_ONLY_TIP: &str = "read-only for this forge in this version: nothing is posted";

/// The tooltip of the "experimental" badge next to Submit while inline posting is on. Only
/// Bitbucket's switch is the environment variable; other forges have their own gate.
pub fn inline_experiment_tip(forge: Forge) -> &'static str {
    match forge {
        Forge::BitbucketCloud => {
            "NIT_INLINE_EXPERIMENT=1: inline comments are enabled for the anchoring \
             experiment; placement is not verified."
        }
        Forge::GitHub | Forge::GitLab => {
            "Inline comments on this forge are new: check where each one lands in the \
             browser after posting."
        }
    }
}

/// A disabled "Read-only" button whose tooltip says why (in place of the write buttons).
pub fn read_only_badge(ui: &mut Ui) {
    ui.add_enabled(false, egui::Button::new("Read-only"))
        .on_disabled_hover_text(READ_ONLY_TIP);
}

/// The bottom bar: "Drafts (N)", Submit, progress, errors and the list of drafts.
pub fn drafts_bar(ui: &mut Ui, c: &Composer, acts: &mut Vec<Act>) {
    ui.add_space(4.0);
    if c.read_only {
        read_only_badge(ui);
        return;
    }
    ui.horizontal(|ui| {
        let n = c.drafts.len();
        ui.label(RichText::new(format!("Drafts ({n})")).strong());
        let postable = c.postable().len();
        // refused inline drafts still enable Submit: the click explains the refusal
        let ok = !c.running() && (postable > 0 || c.refused_inline() > 0) && c.can_post();
        let resp = ui.add_enabled(ok, egui::Button::new("Submit drafts"));
        let resp = if c.can_post() {
            resp.on_disabled_hover_text("No drafts to post")
        } else {
            resp.on_disabled_hover_text("Posting needs a signed-in account")
        };
        if resp.clicked() {
            acts.push(Act::Submit);
        }
        if c.inline_enabled() {
            ui.label(
                RichText::new(" experimental ")
                    .size(11.0)
                    .color(Color32::BLACK)
                    .background_color(AMBER),
            )
            .on_hover_text(inline_experiment_tip(c.forge));
        }
        if let Some(p) = c.progress() {
            ui.spinner();
            ui.label(p);
            let label = if c.stopping() {
                "Stopping..."
            } else {
                "Stop after this comment"
            };
            if ui
                .add_enabled(!c.stopping(), egui::Button::new(label).small())
                .on_hover_text("The comment being posted finishes; the rest stay drafts")
                .clicked()
            {
                acts.push(Act::StopBatch);
            }
        }
        let held = c.held_inline();
        if held > 0 {
            ui.colored_label(
                AMBER,
                format!(
                    "{held} inline draft(s) held: the pull request changed, Reload to post them."
                ),
            );
        }
        if let Some(e) = &c.error {
            ui.colored_label(RED, e);
            if ui.small_button("Dismiss").clicked() {
                acts.push(Act::DismissError);
            }
        } else if let Some(n) = &c.notice {
            ui.colored_label(AMBER, n);
            if ui.small_button("Dismiss").clicked() {
                acts.push(Act::DismissError);
            }
        }
    });
    if c.drafts.is_empty() {
        ui.add_space(2.0);
        return;
    }
    egui::CollapsingHeader::new("Saved drafts")
        .id_salt("review-drafts-list")
        .default_open(true)
        .show_unindented(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("review-drafts-scroll")
                .max_height(96.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for d in &c.drafts {
                        draft_row(ui, c, d, acts);
                    }
                });
        });
}

fn draft_row(ui: &mut Ui, c: &Composer, d: &Draft, acts: &mut Vec<Act>) {
    let state = c.state(d.id);
    let editable = !c.running() && state != DraftState::Posting;
    ui.horizontal(|ui| {
        ui.label(RichText::new(kind_label(d.kind)).color(DRAFT).strong());
        if let Some(t) = d.comment_type {
            super::types_ui::type_label(ui, t);
        }
        if ui
            .link(RichText::new(describe(d)).monospace())
            .on_hover_text("Show it")
            .clicked()
        {
            acts.push(Act::Reveal(d.id));
        }
        ui.label(RichText::new(preview(&d.text)).color(QUIET));
        if state == DraftState::Posting {
            ui.colored_label(AMBER, "Posting...");
        }
        if let Some((note, color)) = state.note(d) {
            ui.colored_label(color, note);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(editable, egui::Button::new("Delete").small())
                .clicked()
            {
                acts.push(Act::Delete(d.id));
            }
            if state != DraftState::PostedNotRemoved
                && ui
                    .add_enabled(editable, egui::Button::new("Edit").small())
                    .clicked()
            {
                acts.push(Act::Reveal(d.id));
                acts.push(Act::Edit(d.id));
            }
            let off = d.kind == DraftKind::Inline && !c.inline_enabled();
            if (off || matches!(state, DraftState::Blocked(_) | DraftState::Outdated))
                && d.kind == DraftKind::Inline
                && ui
                    .add_enabled(editable, egui::Button::new(AS_FILE).small())
                    .clicked()
            {
                acts.push(Act::AsFile(d.id));
            }
            if off
                && ui
                    .add_enabled(editable, egui::Button::new(AS_PR).small())
                    .clicked()
            {
                acts.push(Act::AsPr(d.id));
            }
            if let DraftState::Unconfirmed { checked, .. } = state {
                if ui
                    .add_enabled(editable && checked, egui::Button::new(POST_AGAIN).small())
                    .on_disabled_hover_text("Check comments first")
                    .clicked()
                {
                    acts.push(Act::Rearm(d.id));
                }
                if ui
                    .add_enabled(editable, egui::Button::new(CHECK_COMMENTS).small())
                    .on_hover_text("Fetch the comments again to see whether it was posted")
                    .clicked()
                {
                    acts.push(Act::CheckComments(d.id));
                }
            }
        });
    });
}

/// "Post N comments to Bitbucket?" with every listed draft; nothing posts without it.
pub fn confirm_dialog(ctx: &egui::Context, c: &Composer, acts: &mut Vec<Act>) {
    let Some(ids) = &c.confirm else { return };
    let n = ids.len();
    let resp = egui::Modal::new(Id::unique("review-submit-confirm")).show(ctx, |ui| {
        ui.set_max_width(720.0);
        let noun = if n == 1 { "comment" } else { "comments" };
        ui.heading(format!("Post {n} {noun} to Bitbucket?"));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for d in ids.iter().filter_map(|id| c.draft(*id)) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(kind_label(d.kind)).color(DRAFT).strong());
                        if let Some(t) = d.comment_type {
                            super::types_ui::type_label(ui, t);
                        }
                        ui.label(RichText::new(describe(d)).monospace());
                        ui.label(RichText::new(preview(&d.text)).color(QUIET));
                    });
                    if d.kind == DraftKind::Inline && !INLINE_VERIFIED && c.forge != Forge::GitHub {
                        ui.label(
                            RichText::new(INLINE_UNVERIFIED_NOTE)
                                .color(AMBER)
                                .size(12.0),
                        );
                    }
                    ui.add_space(2.0);
                }
            });
        let held = c.held_inline();
        if held > 0 {
            ui.colored_label(
                AMBER,
                format!("{held} inline draft(s) are held until Reload: the pull request changed."),
            );
        }
        let refused = c.refused_inline();
        if refused > 0 {
            ui.colored_label(AMBER, inline_disabled_message(refused));
        }
        ui.label(
            RichText::new(
                "Comments are posted one at a time; posting stops at the first failure and \
                 unposted drafts stay saved.",
            )
            .color(QUIET),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(format!("Post {n} {noun}")).clicked() {
                acts.push(Act::Confirm);
            }
            if ui.button("Cancel").clicked() {
                acts.push(Act::CancelSubmit);
            }
        });
    });
    if resp.should_close() && !acts.contains(&Act::Confirm) {
        acts.push(Act::CancelSubmit);
    }
}

/// "Thread resolved. Undo" for `UNDO_FOR`.
pub fn toast(ctx: &egui::Context, c: &Composer, acts: &mut Vec<Act>) {
    let Some(t) = &c.toast else { return };
    let left = UNDO_FOR.saturating_sub(t.at.elapsed());
    ctx.request_repaint_after(left.min(Duration::from_millis(500)));
    egui::Area::new(Id::unique("review-resolve-toast"))
        .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -70.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let what = if t.resolved {
                        "Thread resolved."
                    } else {
                        "Thread unresolved."
                    };
                    ui.label(what);
                    if c.is_resolving(&t.root_id) {
                        ui.spinner();
                    }
                    if t.can_undo
                        && ui
                            .add_enabled(!c.is_resolving(&t.root_id), egui::Button::new("Undo"))
                            .clicked()
                    {
                        acts.push(Act::Undo);
                    }
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use prcore::comments::build_threads;
    use std::time::Duration;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repo() -> RepoRef {
        RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: "acme".into(),
            slug: "api".into(),
        }
    }

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>) -> Row {
        Row {
            kind,
            old_line: old,
            new_line: new,
            text: "x".into(),
        }
    }

    /// a.rs: ctx 10/10, del old 11, add new 11, ctx 12/12.
    fn source(head: &str) -> RowSource {
        let head = head.to_string();
        Arc::new(move |path| {
            let rows = if path == "a.rs" {
                vec![
                    row(RowKind::HunkHeader, None, None),
                    row(RowKind::Context, Some(10), Some(10)),
                    row(RowKind::Del, Some(11), None),
                    row(RowKind::Add, None, Some(11)),
                    row(RowKind::Context, Some(12), Some(12)),
                ]
            } else {
                vec![]
            };
            Ok(FileRows {
                repo: repo(),
                pr_id: 7,
                head: head.clone(),
                merge_base: MERGE_BASE.into(),
                rows: Arc::new(rows),
                binary: path.ends_with(".png"),
            })
        })
    }

    fn mem_store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    fn composer(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>) -> Composer {
        let mut c = Composer::new(repo(), 7, store);
        c.head = HEAD.into();
        c.inline_enabled = true;
        c.connect(poster.clone(), source(HEAD), None);
        c
    }

    fn write(c: &mut Composer, target: Target, text: &str) -> i64 {
        c.open_editor(target);
        c.editor.as_mut().unwrap().text = text.into();
        assert!(c.save_editor(), "saved");
        c.drafts.last().unwrap().id
    }

    fn inline(path: &str, side: Side, line: u32) -> Target {
        Target::Inline {
            path: path.into(),
            side,
            line,
            start: None,
        }
    }

    /// Runs a confirmed submit to the end.
    fn submit(c: &mut Composer, comments: &mut Comments) -> PollOutcome {
        c.request_submit();
        assert!(c.confirm.is_some(), "confirmation opens");
        assert!(c.confirm_submit());
        wait(c, comments)
    }

    fn wait(c: &mut Composer, comments: &mut Comments) -> PollOutcome {
        let start = Instant::now();
        let mut all = PollOutcome::default();
        loop {
            let o = c.poll(comments);
            all.refresh |= o.refresh;
            all.stale |= o.stale;
            all.threads_changed |= o.threads_changed;
            if !c.running() && !c.any_resolving() {
                return all;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "batch hung");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stored(store: &Arc<Mutex<Store>>) -> Vec<Draft> {
        store.lock().unwrap().list_drafts(&repo(), 7).unwrap()
    }

    fn texts(v: &[NewComment]) -> Vec<String> {
        v.iter()
            .map(|c| match c {
                NewComment::Pr { text }
                | NewComment::File { text, .. }
                | NewComment::Inline { text, .. }
                | NewComment::Reply { text, .. } => text.clone(),
            })
            .collect()
    }

    #[test]
    fn submit_posts_in_order_and_deletes_on_success() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(&HEAD[..12]));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, Target::Pr, "one");
        write(&mut c, Target::File("a.rs".into()), "two");
        write(&mut c, inline("a.rs", Side::New, 11), "three");
        write(&mut c, inline("a.rs", Side::Old, 11), "four");
        assert_eq!(stored(&store).len(), 4);
        submit(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["one", "two", "three", "four"]
        );
        assert!(stored(&store).is_empty(), "deleted after each 2xx");
        assert!(c.drafts.is_empty());
        assert_eq!(comments.threads.len(), 4, "shown as threads at once");
        let a = comments.threads[2].root.anchor.as_ref().unwrap();
        assert_eq!((a.path.as_str(), a.to), ("a.rs", Some(11)));
    }

    #[test]
    fn failed_post_keeps_draft_and_stops_batch_not_double_posts_on_retry() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, Target::Pr, "one");
        let two = write(&mut c, Target::Pr, "two");
        write(&mut c, Target::Pr, "three");
        poster
            .script
            .lock()
            .unwrap()
            .extend([None, Some(PostError::Network("network error".into()))]);
        submit(&mut c, &mut comments);
        assert_eq!(texts(&poster.attempts.lock().unwrap()), ["one", "two"]);
        let left: Vec<String> = stored(&store).into_iter().map(|d| d.text).collect();
        assert_eq!(left, ["two", "three"], "failed and unattempted drafts stay");
        assert!(matches!(c.state(two), DraftState::Failed(e) if e.contains("network")));
        // retry: only the drafts still present, each posted exactly once overall
        submit(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["one", "two", "three"]
        );
        assert!(stored(&store).is_empty() && c.drafts.is_empty());
        assert_eq!(comments.threads.len(), 3);
    }

    #[test]
    fn batch_stops_after_first_error() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let first = write(&mut c, Target::Pr, "one");
        write(&mut c, Target::File("a.rs".into()), "two");
        write(
            &mut c,
            Target::Reply {
                thread: ThreadId::from(5),
            },
            "three",
        );
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Rejected("access denied".into())));
        submit(&mut c, &mut comments);
        assert_eq!(poster.attempts.lock().unwrap().len(), 1, "nothing after it");
        assert!(poster.posted.lock().unwrap().is_empty());
        assert_eq!(stored(&store).len(), 3);
        assert!(matches!(c.state(first), DraftState::Failed(e) if e.contains("access denied")));
        assert!(!c.running(), "Submit is enabled again");
    }

    const MERGE_BASE: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn inline_check_uses_the_forges_anchor_model_and_base() {
        // a forge anchoring to diff refs: same head but another (or an unknown) merge base
        // never posts, and the draft stays; the reviewed base posts
        for (base, posted) in [
            (Some("another-base"), false),
            (None, false),
            (Some(MERGE_BASE), true),
        ] {
            let store = mem_store();
            let poster = Arc::new(MemoryPoster::new(HEAD));
            *poster.anchor.lock().unwrap() = AnchorModel::DiffRefs;
            *poster.server_base.lock().unwrap() = base.map(Into::into);
            let mut c = composer(store.clone(), &poster);
            let mut comments = Comments::new();
            let id = write(&mut c, inline("a.rs", Side::New, 11), "on a line");
            submit(&mut c, &mut comments);
            if posted {
                assert_eq!(poster.posted.lock().unwrap().len(), 1);
                assert!(stored(&store).is_empty());
            } else {
                assert!(poster.attempts.lock().unwrap().is_empty(), "never posted");
                assert_eq!(c.state(id), DraftState::Blocked(InlineBlock::BaseMismatch));
                assert_eq!(stored(&store).len(), 1, "still a draft");
            }
        }
    }

    #[test]
    fn every_post_carries_the_reviewed_revision() {
        // the provider gets exactly what was fetched: an inline draft its file's head and
        // merge base, a PR-level draft the review's head (no base needed)
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        *poster.anchor.lock().unwrap() = AnchorModel::DiffRefs;
        *poster.server_base.lock().unwrap() = Some(MERGE_BASE.into());
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, inline("a.rs", Side::New, 11), "on a line");
        write(&mut c, Target::Pr, "overall");
        submit(&mut c, &mut comments);
        assert_eq!(
            *poster.revisions.lock().unwrap(),
            vec![
                PostRevision {
                    head: HEAD.into(),
                    merge_base: MERGE_BASE.into()
                },
                PostRevision {
                    head: HEAD.into(),
                    merge_base: String::new()
                },
            ]
        );
    }

    #[test]
    fn bitbucket_inline_check_ignores_the_base() {
        // Bitbucket (current diff): whatever base the server reports, the head decides
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        *poster.server_base.lock().unwrap() = Some("unrelated".into());
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, inline("a.rs", Side::New, 11), "on a line");
        submit(&mut c, &mut comments);
        assert_eq!(poster.posted.lock().unwrap().len(), 1);
    }

    #[test]
    fn inline_blocked_when_head_changed_offers_file_level_only() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new("fedcba9876543210"));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, inline("a.rs", Side::New, 11), "on a line");
        let out = submit(&mut c, &mut comments);
        assert!(poster.attempts.lock().unwrap().is_empty(), "never posted");
        assert_eq!(c.state(id), DraftState::Blocked(InlineBlock::HeadMismatch));
        assert_eq!(stored(&store).len(), 1, "still a draft");
        assert!(out.stale && c.stale, "the screen learns the PR moved");
        // only an explicit click turns it into a file draft; still nothing is posted
        c.as_file_comment(id);
        let left = stored(&store);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, DraftKind::File);
        assert_eq!(left[0].path.as_deref(), Some("a.rs"));
        assert_eq!(left[0].text, "(line 11) on a line");
        assert!(poster.attempts.lock().unwrap().is_empty());
        // the file draft posts fine while the PR is stale
        submit(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["(line 11) on a line"]
        );
    }

    #[test]
    fn inline_blocked_for_line_not_in_diff() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let far = write(&mut c, inline("a.rs", Side::New, 500), "far away");
        let wrong_side = write(&mut c, inline("a.rs", Side::Old, 11), "old side ok");
        let added_on_old = write(&mut c, inline("a.rs", Side::New, 12), "ctx new 12");
        let bin = write(&mut c, inline("logo.png", Side::New, 1), "binary");
        submit(&mut c, &mut comments);
        let blocked = |id| c.state(id);
        assert_eq!(
            blocked(far),
            DraftState::Blocked(InlineBlock::LineNotInDiff)
        );
        assert_eq!(blocked(bin), DraftState::Blocked(InlineBlock::BinaryFile));
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["old side ok", "ctx new 12"],
            "a block is not a failure: the valid drafts still post"
        );
        let left: Vec<i64> = stored(&store).into_iter().map(|d| d.id).collect();
        assert_eq!(left, [far, bin]);
        assert!(!c
            .drafts
            .iter()
            .any(|d| d.id == wrong_side || d.id == added_on_old));
    }

    #[test]
    fn drafts_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.sqlite");
        let poster = Arc::new(MemoryPoster::new(HEAD));
        {
            let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
            let mut c = composer(store, &poster);
            write(&mut c, inline("a.rs", Side::New, 11), "kept");
            write(
                &mut c,
                Target::Reply {
                    thread: ThreadId::from(3),
                },
                "reply kept",
            );
        }
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let c = composer(store, &poster);
        assert_eq!(c.drafts.len(), 2);
        let d = &c.drafts[0];
        assert_eq!(
            (d.kind, d.path.as_deref(), d.side, d.line),
            (DraftKind::Inline, Some("a.rs"), Some(Side::New), Some(11))
        );
        assert_eq!(d.source_commit, HEAD, "the fetched head at draft time");
        assert_eq!(c.drafts[1].reply_to, Some(ThreadId::from(3)));
    }

    #[test]
    fn empty_draft_not_saved() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        c.open_editor(Target::Pr);
        c.editor.as_mut().unwrap().text = "  \n\t ".into();
        assert!(!c.save_editor());
        assert!(c.editor.is_none() && c.drafts.is_empty());
        assert!(stored(&store).is_empty());
        // switching composers keeps typed text as a draft but drops empty ones
        c.open_editor(Target::Pr);
        c.open_editor(Target::File("a.rs".into()));
        assert!(stored(&store).is_empty());
        c.editor.as_mut().unwrap().text = "typed".into();
        c.open_editor(Target::Pr);
        assert_eq!(stored(&store).len(), 1);
        // emptying an existing draft does not change it
        let id = c.drafts[0].id;
        c.cancel_editor();
        c.edit_draft(id);
        c.editor.as_mut().unwrap().text.clear();
        assert!(!c.save_editor());
        assert_eq!(stored(&store)[0].text, "typed");
    }

    fn comment(id: u64, resolved: bool) -> Comment {
        let t = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Comment {
            id: id.into(),
            parent_id: None,
            author: UserRef {
                id: "{a}".into(),
                display_name: "Ana".into(),
                login: None,
            },
            body: "b".into(),
            created_on: t,
            updated_on: t,
            deleted: false,
            resolved,
            anchor: None,
            author_is_bot: false,
            thread: Some(prcore::comments::ThreadId::from(id)),
            can_resolve: true,
        }
    }

    #[test]
    fn resolve_calls_poster_and_updates_thread_with_undo() {
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        let mut comments = Comments::new();
        comments.set(build_threads(vec![comment(1, false), comment(2, false)]), 0);
        c.toggle_resolved(&mut comments, &1.into());
        assert!(comments.threads[0].root.resolved, "optimistic");
        assert!(c.is_resolving(&1.into()));
        c.toggle_resolved(&mut comments, &1.into()); // disabled while in flight
        wait(&mut c, &mut comments);
        assert_eq!(
            *poster.resolves.lock().unwrap(),
            [(ThreadId::from(1), true)]
        );
        assert!(comments.threads[0].root.resolved);
        assert!(c.toast.as_ref().is_some_and(|t| t.can_undo && t.resolved));
        c.undo(&mut comments);
        assert!(!comments.threads[0].root.resolved);
        wait(&mut c, &mut comments);
        assert_eq!(
            *poster.resolves.lock().unwrap(),
            [(ThreadId::from(1), true), (ThreadId::from(1), false)]
        );
        assert!(c.toast.as_ref().is_some_and(|t| !t.can_undo));
        // a refusal reverts the thread and says so
        poster
            .resolve_script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Rejected("access denied".into())));
        c.toggle_resolved(&mut comments, &2.into());
        assert!(comments.threads[1].root.resolved);
        wait(&mut c, &mut comments);
        assert!(!comments.threads[1].root.resolved, "reverted");
        assert!(c
            .error
            .as_deref()
            .is_some_and(|e| e.contains("access denied")));
        assert!(c.toast.is_none());
        // the toast goes after UNDO_FOR
        c.toggle_resolved(&mut comments, &1.into());
        wait(&mut c, &mut comments);
        c.toast.as_mut().unwrap().at = Instant::now() - UNDO_FOR;
        c.poll(&mut comments);
        assert!(c.toast.is_none());
    }

    /// A resolve whose answer was lost may have landed: no failure, the comments are
    /// refetched and the server's state is adopted; it shows as done when it applied.
    #[test]
    fn resolve_with_unknown_outcome_refreshes_instead_of_failing() {
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        let mut comments = Comments::new();
        comments.set(build_threads(vec![comment(1, false), comment(2, false)]), 0);
        poster
            .resolve_script
            .lock()
            .unwrap()
            .extend([Some(PostError::Unconfirmed), Some(PostError::Unconfirmed)]);
        // it applied on the server
        c.toggle_resolved(&mut comments, &1.into());
        let out = wait(&mut c, &mut comments);
        assert!(out.refresh, "the comments are fetched again");
        assert!(c.error.is_none(), "not shown as a failure");
        assert!(c
            .notice
            .as_deref()
            .is_some_and(|n| n.starts_with("Not confirmed")));
        assert!(comments.threads[0].root.resolved, "not reverted meanwhile");
        let mut server = comment(1, true);
        server.body = "b".into();
        comments.set(build_threads(vec![server, comment(2, false)]), 0);
        c.resolves_checked(&comments);
        assert!(c.notice.is_none(), "the refresh shows it applied");
        assert!(c.error.is_none());
        // it did not apply: the server's state is shown, and said to be unconfirmed
        c.toggle_resolved(&mut comments, &2.into());
        let out = wait(&mut c, &mut comments);
        assert!(out.refresh);
        comments.set(build_threads(vec![comment(1, true), comment(2, false)]), 0);
        c.reapply_pending(&mut comments);
        c.resolves_checked(&comments);
        assert!(!comments.threads[1].root.resolved, "the server's state");
        assert!(c.error.is_none());
        let n = c.notice.clone().unwrap_or_default();
        assert!(
            n.contains("Not confirmed") && n.contains("refreshed"),
            "{n}"
        );
    }

    fn posting_ids(store: &Arc<Mutex<Store>>) -> Vec<i64> {
        store.lock().unwrap().posting_draft_ids(&repo(), 7).unwrap()
    }

    const UNKNOWN: DraftState = DraftState::Unconfirmed {
        checked: false,
        looks_posted: false,
    };

    #[test]
    fn timeout_after_send_is_unconfirmed_and_excluded_from_postable() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, Target::Pr, "one");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
        submit(&mut c, &mut comments);
        assert_eq!(c.state(id), UNKNOWN);
        let d = c.draft(id).unwrap().clone();
        assert_eq!(c.state(id).note(&d).unwrap().0, UNCONFIRMED_TEXT);
        assert_eq!(
            UNCONFIRMED_TEXT,
            "Not confirmed: the connection failed after sending. Refresh comments to check \
             before posting again."
        );
        assert!(c.postable().is_empty(), "not offered again");
        assert_eq!(stored(&store).len(), 1, "kept as a draft");
        assert_eq!(posting_ids(&store), [id], "the marker stays");
        c.request_submit();
        assert!(c.confirm.is_none(), "nothing to submit");
        // editing it does not make it postable either
        c.edit_draft(id);
        c.editor.as_mut().unwrap().text = "one, edited".into();
        assert!(c.save_editor());
        assert_eq!(c.state(id), UNKNOWN);
        assert!(c.postable().is_empty());
        assert_eq!(poster.attempts.lock().unwrap().len(), 1);
    }

    #[test]
    fn connect_failure_is_definite() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, Target::Pr, "one");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Network("network error".into())));
        submit(&mut c, &mut comments);
        let d = c.draft(id).unwrap().clone();
        assert!(c.state(id).note(&d).unwrap().0.starts_with("Not posted"));
        assert!(posting_ids(&store).is_empty(), "the marker is cleared");
        assert_eq!(c.postable(), [id], "can be submitted again");
    }

    #[test]
    fn restart_with_posting_marker_is_unconfirmed() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let id = write(&mut composer(store.clone(), &poster), Target::Pr, "one");
        // a crash between "posting started" and the answer
        store.lock().unwrap().mark_draft_posting(id).unwrap();
        let c = composer(store.clone(), &poster);
        assert_eq!(c.state(id), UNKNOWN);
        assert!(c.postable().is_empty());
        // while this process is still posting it, it shows as posting
        let reg = InFlight::default();
        reg.lock().unwrap().insert(id);
        let other = Composer::with_in_flight(repo(), 7, store.clone(), reg);
        assert_eq!(other.state(id), DraftState::Posting);
    }

    #[test]
    fn batch_stops_after_unconfirmed() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, Target::Pr, "one");
        let two = write(&mut c, Target::Pr, "two");
        let three = write(&mut c, Target::Pr, "three");
        poster
            .script
            .lock()
            .unwrap()
            .extend([None, Some(PostError::Unconfirmed)]);
        submit(&mut c, &mut comments);
        assert_eq!(texts(&poster.attempts.lock().unwrap()), ["one", "two"]);
        assert_eq!(c.state(two), UNKNOWN);
        assert_eq!(c.state(three), DraftState::Idle);
        assert_eq!(c.postable(), [three]);
    }

    #[test]
    fn post_again_requires_explicit_rearm() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, Target::Pr, "same text");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
        submit(&mut c, &mut comments);
        assert!(!c.rearm(id), "not before the comments were checked");
        assert_eq!(c.state(id), UNKNOWN);
        // "Check comments": a refresh is asked for; its result is compared
        assert!(c.check_comments(id));
        let me = UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        };
        let mut mine = comment(50, false);
        mine.author = me.clone();
        mine.body = "same text".into();
        let mut other = comment(51, false);
        other.body = "same text".into();
        comments.set(build_threads(vec![other.clone()]), 0);
        c.comments_checked(&comments, Some(&me));
        assert_eq!(
            c.state(id),
            DraftState::Unconfirmed {
                checked: true,
                looks_posted: false
            },
            "someone else's identical text is not mine"
        );
        assert!(c.check_comments(id));
        comments.set(build_threads(vec![other, mine]), 0);
        c.comments_checked(&comments, Some(&me));
        let st = c.state(id);
        assert_eq!(
            st,
            DraftState::Unconfirmed {
                checked: true,
                looks_posted: true
            }
        );
        let d = c.draft(id).unwrap().clone();
        assert!(st.note(&d).unwrap().0.contains(LOOKS_POSTED));
        assert!(c.postable().is_empty(), "still not postable: hint only");
        assert_eq!(stored(&store).len(), 1, "never deleted automatically");
        // "Post again anyway"
        assert!(c.rearm(id));
        assert_eq!(c.state(id), DraftState::Idle);
        assert!(posting_ids(&store).is_empty());
        assert_eq!(c.postable(), [id]);
        submit(&mut c, &mut comments);
        assert_eq!(poster.attempts.lock().unwrap().len(), 2);
        assert!(stored(&store).is_empty());
    }

    /// "Check comments" must not miss a comment that was posted: the text sent counts even
    /// after the draft was edited, and line endings or trailing blanks the server normalised
    /// do not hide it. A check that found nothing says so.
    #[test]
    fn check_comments_finds_the_sent_text_and_reports_a_miss() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write(&mut c, Target::Pr, "line one\r\nline two\n");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
        submit(&mut c, &mut comments);
        let me = UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        };
        // nothing there yet: the note says the check found nothing
        assert!(c.check_comments(id));
        comments.set(build_threads(vec![]), 0);
        c.comments_checked(&comments, Some(&me));
        let d = c.draft(id).unwrap().clone();
        let note = c.state(id).note(&d).unwrap().0;
        assert_ne!(note, UNCONFIRMED_TEXT, "a finished check is reported");
        assert!(note.contains(NOT_FOUND), "{note}");
        // edited after the unknown outcome; the server kept the sent text, normalised
        c.edit_draft(id);
        c.editor.as_mut().unwrap().text = "something else".into();
        assert!(c.save_editor());
        let mut mine = comment(60, false);
        mine.author = me.clone();
        mine.body = "line one\nline two".into();
        assert!(c.check_comments(id));
        comments.set(build_threads(vec![mine]), 0);
        c.comments_checked(&comments, Some(&me));
        assert_eq!(
            c.state(id),
            DraftState::Unconfirmed {
                checked: true,
                looks_posted: true
            }
        );
        assert!(c.postable().is_empty(), "still a hint only");
    }

    fn write_typed(c: &mut Composer, target: Target, t: CommentType, text: &str) -> i64 {
        c.open_editor(target);
        let e = c.editor.as_mut().unwrap();
        e.text = text.into();
        e.comment_type = Some(t);
        assert!(c.save_editor(), "saved");
        c.drafts.last().unwrap().id
    }

    #[test]
    fn typed_draft_is_posted_with_one_prefix_also_on_retry() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write_typed(&mut c, Target::Pr, CommentType::Nit, "rename this");
        write(&mut c, Target::Pr, "untyped");
        // the stored text has no prefix: it is added when posting only
        assert_eq!(stored(&store)[0].text, "rename this");
        assert_eq!(stored(&store)[0].comment_type, Some(CommentType::Nit));
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Network("network error".into())));
        submit(&mut c, &mut comments);
        assert!(matches!(c.state(id), DraftState::Failed(_)));
        assert_eq!(
            stored(&store)[0].text,
            "rename this",
            "unchanged by the try"
        );
        submit(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.attempts.lock().unwrap()),
            ["**nit:** rename this", "**nit:** rename this", "untyped"]
        );
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["**nit:** rename this", "untyped"]
        );
        assert_eq!(comments.threads[0].root.body, "**nit:** rename this");
    }

    #[test]
    fn editing_keeps_or_changes_the_type() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let id = write_typed(&mut c, Target::Pr, CommentType::Question, "why?");
        c.edit_draft(id);
        assert_eq!(
            c.editor.as_ref().unwrap().comment_type,
            Some(CommentType::Question),
            "the editor opens with the draft's type"
        );
        assert_eq!(
            c.editor.as_ref().unwrap().text,
            "why?",
            "no prefix in the text"
        );
        // Tab in the composer: the next type
        let e = c.editor.as_mut().unwrap();
        e.comment_type = CommentType::cycle(e.comment_type, true);
        assert!(c.save_editor());
        assert_eq!(
            stored(&store)[0].comment_type,
            Some(CommentType::Suggestion)
        );
        c.edit_draft(id);
        c.editor.as_mut().unwrap().comment_type = None;
        assert!(c.save_editor());
        assert_eq!(stored(&store)[0].comment_type, None);
        // a new composer starts without a type
        c.open_editor(Target::File("a.rs".into()));
        assert_eq!(c.editor.as_ref().unwrap().comment_type, None);
    }

    /// "Check comments" compares with the exact text sent, prefix included: also after a
    /// restart and after the draft's type was changed.
    #[test]
    fn check_comments_matches_the_prefixed_text_that_was_sent() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let id = write_typed(&mut c, Target::Pr, CommentType::Issue, "leaks the handle");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
        submit(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.attempts.lock().unwrap()),
            ["**issue:** leaks the handle"]
        );
        let me = UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        };
        let mine = |id: u64, body: &str| {
            let mut m = comment(id, false);
            m.author = me.clone();
            m.body = body.into();
            m
        };
        // the bare text is not what was sent
        assert!(c.check_comments(id));
        comments.set(build_threads(vec![mine(70, "leaks the handle")]), 0);
        c.comments_checked(&comments, Some(&me));
        assert_eq!(
            c.state(id),
            DraftState::Unconfirmed {
                checked: true,
                looks_posted: false
            }
        );
        // the type was changed afterwards: what was sent still counts
        c.edit_draft(id);
        c.editor.as_mut().unwrap().comment_type = Some(CommentType::Nit);
        assert!(c.save_editor());
        assert!(c.check_comments(id));
        comments.set(
            build_threads(vec![mine(71, "**issue:** leaks the handle\n")]),
            0,
        );
        c.comments_checked(&comments, Some(&me));
        assert!(matches!(
            c.state(id),
            DraftState::Unconfirmed {
                looks_posted: true,
                ..
            }
        ));
        // after a restart the sent text is rebuilt from the stored draft, prefix included
        let mut again = composer(store.clone(), &poster);
        assert!(again.check_comments(id));
        comments.set(
            build_threads(vec![mine(72, "**nit:** leaks the handle")]),
            0,
        );
        again.comments_checked(&comments, Some(&me));
        assert!(matches!(
            again.state(id),
            DraftState::Unconfirmed {
                looks_posted: true,
                ..
            }
        ));
        assert!(again.postable().is_empty(), "a hint only");
    }

    #[test]
    fn submit_requires_confirmation_flag() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        assert!(!c.confirm_submit(), "no confirmation open: nothing starts");
        write(&mut c, Target::Pr, "one");
        c.request_submit();
        assert_eq!(c.confirm.as_deref(), Some(&[c.drafts[0].id][..]));
        std::thread::sleep(Duration::from_millis(100));
        c.poll(&mut comments);
        assert!(
            poster.attempts.lock().unwrap().is_empty(),
            "no post before confirm"
        );
        c.cancel_submit();
        assert!(!c.confirm_submit());
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert_eq!(stored(&store).len(), 1);
        submit(&mut c, &mut comments);
        assert_eq!(poster.posted.lock().unwrap().len(), 1);
        // without a poster there is no confirmation at all
        let mut offline = Composer::new(repo(), 7, store);
        write(&mut offline, Target::Pr, "x");
        offline.request_submit();
        assert!(offline.confirm.is_none() && offline.error.is_some());
    }

    #[test]
    fn posted_unreadable_deletes_draft_and_refreshes() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        write(&mut c, Target::Pr, "one");
        write(&mut c, Target::Pr, "two");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::PostedUnreadable));
        let out = submit(&mut c, &mut comments);
        assert!(out.refresh, "the posted comment is fetched");
        assert!(
            stored(&store).is_empty(),
            "treated as posted, batch went on"
        );
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "two"]);
        assert!(c.drafts.is_empty());
    }

    #[test]
    fn stale_disables_inline_submit_only() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        let line = write(&mut c, inline("a.rs", Side::New, 11), "inline");
        let pr = write(&mut c, Target::Pr, "pr");
        let file = write(&mut c, Target::File("a.rs".into()), "file");
        let reply = write(
            &mut c,
            Target::Reply {
                thread: ThreadId::from(1),
            },
            "reply",
        );
        c.stale = true;
        assert_eq!(c.postable(), [pr, file, reply]);
        assert_eq!(c.held_inline(), 1);
        c.request_submit();
        assert!(!c.confirm.as_ref().unwrap().contains(&line));
        assert!(c.confirm_submit());
        wait(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["pr", "file", "reply"]
        );
        assert_eq!(
            stored(&store).into_iter().map(|d| d.id).collect::<Vec<_>>(),
            [line]
        );
        // with only inline drafts left, Submit has nothing to offer
        c.request_submit();
        assert!(c.confirm.is_none());
    }

    /// A composer as the app builds it without NIT_INLINE_EXPERIMENT: inline posting off.
    fn composer_inline_off(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>) -> Composer {
        let mut c = composer(store, poster);
        c.inline_enabled = false;
        c
    }

    #[test]
    fn inline_disabled_refuses_inline_drafts_but_posts_the_rest() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer_inline_off(store.clone(), &poster);
        let mut comments = Comments::new();
        let line = write(&mut c, inline("a.rs", Side::New, 11), "inline");
        let pr = write(&mut c, Target::Pr, "pr");
        let file = write(&mut c, Target::File("a.rs".into()), "file");
        let reply = write(&mut c, Target::Reply { thread: 1.into() }, "reply");
        assert_eq!(c.postable(), [pr, file, reply]);
        c.request_submit();
        let msg = c.error.clone().expect("the refusal is explained");
        assert!(msg.contains("1 inline draft"), "{msg}");
        assert!(
            msg.contains("file") && msg.contains("PR"),
            "offers conversion: {msg}"
        );
        assert_eq!(c.confirm.as_deref(), Some(&[pr, file, reply][..]));
        assert!(c.confirm_submit());
        wait(&mut c, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["pr", "file", "reply"]
        );
        let left = stored(&store);
        assert_eq!(left.len(), 1, "the inline draft stays saved");
        assert_eq!((left[0].id, left[0].kind), (line, DraftKind::Inline));
    }

    #[test]
    fn inline_disabled_submit_with_only_inline_drafts_makes_no_network_call() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer_inline_off(store.clone(), &poster);
        let id = write(&mut c, inline("a.rs", Side::New, 11), "inline");
        c.request_submit();
        assert!(c.confirm.is_none(), "nothing to confirm");
        assert!(c.error.as_deref().is_some_and(|m| m.contains("inline")));
        // even a confirmation list forced by hand posts nothing
        c.confirm = Some(vec![id]);
        assert!(!c.confirm_submit());
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert!(poster.posted.lock().unwrap().is_empty());
        assert_eq!(stored(&store).len(), 1);
        assert_eq!(
            c.state(id),
            DraftState::Idle,
            "not silently marked or dropped"
        );
    }

    #[test]
    fn inline_disabled_conversion_is_explicit_and_keeps_the_text() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer_inline_off(store.clone(), &poster);
        let a = write(&mut c, inline("a.rs", Side::New, 11), "one");
        let b = write(&mut c, inline("a.rs", Side::New, 12), "two");
        c.as_file_comment(a);
        c.as_pr_comment(b);
        let mut left: Vec<_> = stored(&store)
            .into_iter()
            .map(|d| (d.kind, d.path, d.text))
            .collect();
        left.sort_by_key(|d| format!("{:?}", d.0));
        assert_eq!(
            left,
            [
                (DraftKind::File, Some("a.rs".into()), "(line 11) one".into()),
                (DraftKind::Pr, None, "(a.rs line 12) two".into()),
            ]
        );
        assert!(poster.attempts.lock().unwrap().is_empty());
    }

    #[test]
    fn inline_enabled_posts_inline_drafts() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        assert!(c.inline_enabled);
        let mut comments = Comments::new();
        write(&mut c, inline("a.rs", Side::New, 11), "inline");
        submit(&mut c, &mut comments);
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["inline"]);
        assert!(c.error.is_none());
        assert!(stored(&store).is_empty());
    }

    #[test]
    fn inline_is_off_unless_the_experiment_flag_is_set() {
        // NIT_INLINE_EXPERIMENT is not set for the test run
        assert!(std::env::var_os("NIT_INLINE_EXPERIMENT").is_none());
        assert!(!inline_posting_enabled());
        let c = Composer::new(repo(), 7, mem_store());
        assert!(!c.inline_enabled);
    }

    #[test]
    fn github_inline_is_gated_by_its_own_const_not_the_bitbucket_flag() {
        let mut c = Composer::new(repo(), 7, mem_store());
        // the Bitbucket gate is off in tests; GitHub follows only its own const
        assert!(!inline_posting_enabled());
        c.set_forge(Forge::GitHub);
        assert_eq!(c.inline_enabled(), providers::github::INLINE_ENABLED_GITHUB);
        assert!(c.inline_enabled());
        write(&mut c, inline("a.rs", Side::New, 11), "inline");
        // posted inline, not held
        assert_eq!(c.refused_inline(), 0);
        assert_eq!(c.postable().len(), 1);
        // and Bitbucket stays held
        c.set_forge(Forge::BitbucketCloud);
        assert_eq!(c.inline_enabled(), inline_posting_enabled());
        assert!(!c.inline_enabled());
    }

    /// GitLab has its own gate too; the Bitbucket experiment flag (off in tests) does not
    /// decide it.
    #[test]
    fn gitlab_inline_is_gated_by_its_own_const() {
        let mut c = Composer::new(repo(), 7, mem_store());
        assert!(!inline_posting_enabled());
        c.set_forge(Forge::GitLab);
        assert_eq!(c.inline_enabled(), providers::gitlab::INLINE_ENABLED_GITLAB);
        assert!(c.inline_enabled());
        write(&mut c, inline("a.rs", Side::New, 11), "inline");
        assert_eq!(c.refused_inline(), 0);
        assert_eq!(c.postable().len(), 1);
    }

    /// The "experimental" badge names the Bitbucket environment variable only for
    /// Bitbucket; the read-only tooltip names no forge.
    #[test]
    fn badge_texts_fit_every_forge() {
        assert!(inline_experiment_tip(Forge::BitbucketCloud).contains(INLINE_EXPERIMENT_VAR));
        for f in [Forge::GitHub, Forge::GitLab] {
            assert!(!inline_experiment_tip(f).contains("NIT_"), "{f:?}");
        }
        for name in ["GitHub", "GitLab", "Bitbucket"] {
            assert!(!READ_ONLY_TIP.contains(name), "{READ_ONLY_TIP}");
        }
        assert!(READ_ONLY_TIP.contains("read-only"));
    }

    #[test]
    fn a_read_only_composer_opens_no_editor() {
        let mut c = Composer::new(repo(), 7, mem_store());
        c.read_only = true;
        for t in [
            Target::Pr,
            Target::Reply {
                thread: ThreadId("PRRT_x".into()),
            },
        ] {
            c.open_editor(t);
            assert!(c.editor.is_none());
        }
        assert!(c
            .error
            .as_deref()
            .is_some_and(|e| e.contains(READ_ONLY_TIP)));
        assert!(c.drafts.is_empty());
    }

    #[test]
    fn draft_for_other_pr_not_shown() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut mine = composer(store.clone(), &poster);
        write(&mut mine, Target::Pr, "mine");
        let mut other = Composer::new(repo(), 8, store.clone());
        write(&mut other, Target::Pr, "other pr");
        let mut elsewhere = Composer::new(
            RepoRef {
                host: prcore::model::BITBUCKET_HOST.into(),
                namespace: "acme".into(),
                slug: "web".into(),
            },
            7,
            store.clone(),
        );
        write(&mut elsewhere, Target::Pr, "other repo");
        let again = composer(store, &poster);
        let shown: Vec<&str> = again.drafts.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(shown, ["mine"]);
    }

    #[test]
    fn plan_puts_drafts_under_their_rows_and_lists_the_rest() {
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        let rows: Vec<Row> = (*source(HEAD)("a.rs").unwrap().rows).clone();
        let threads = build_threads(vec![comment(1, false)]);
        let placement = Placement {
            inline: vec![(3, vec![0])],
            ..Default::default()
        };
        let reply = write(
            &mut c,
            Target::Reply {
                thread: ThreadId::from(1),
            },
            "r",
        );
        let on_add = write(&mut c, inline("a.rs", Side::New, 11), "x");
        let on_del = write(&mut c, inline("a.rs", Side::Old, 11), "y");
        let gone = write(&mut c, inline("a.rs", Side::New, 99), "z");
        c.open_editor(inline("a.rs", Side::Old, 10));
        let p = c.plan("a.rs", &rows, &placement, &threads);
        assert_eq!(
            p.rows.get(&3).unwrap(),
            &[Slot::Thread(0), Slot::Draft(reply), Slot::Draft(on_add)]
        );
        assert_eq!(p.rows.get(&2).unwrap(), &[Slot::Draft(on_del)]);
        assert_eq!(p.rows.get(&1).unwrap(), &[Slot::Editor]);
        assert_eq!(p.unplaced, [gone]);
        assert!(!p.editor_unplaced);
    }

    #[test]
    fn abbreviated_server_head_matches_the_fetched_head() {
        assert_eq!(head_for_check(&HEAD[..12], HEAD), HEAD);
        assert_eq!(head_for_check("", HEAD), "");
        assert_eq!(head_for_check("0123", "0123"), "0123");
        assert_eq!(head_for_check("abc1234", HEAD), "abc1234");
    }

    // ---- safety: older versions, in-flight posts, tombstones ----

    const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn composer_at(store: Arc<Mutex<Store>>, poster: &Arc<MemoryPoster>, head: &str) -> Composer {
        let mut c = Composer::new(repo(), 7, store);
        c.head = head.into();
        c.inline_enabled = true;
        c.connect(poster.clone(), source(head), None);
        c
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn raw_inline(store: &Arc<Mutex<Store>>, source_commit: &str, text: &str) -> i64 {
        let d = Draft {
            id: 0,
            repo: repo(),
            pr_id: 7,
            kind: DraftKind::Inline,
            path: Some("a.rs".into()),
            line: Some(11),
            side: Some(Side::New),
            start: None,
            reply_to: None,
            text: text.into(),
            source_commit: source_commit.into(),
            comment_type: None,
        };
        store.lock().unwrap().save_draft(&d).unwrap()
    }

    #[test]
    fn draft_from_older_head_blocked_after_reload() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let id = {
            let mut old = composer_at(store.clone(), &poster, OLD);
            write(&mut old, inline("a.rs", Side::New, 11), "on the old code")
        };
        // Reload: screen and server both at HEAD, and new line 11 exists there too
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert!(
            poster.attempts.lock().unwrap().is_empty(),
            "never on a guessed line"
        );
        assert_eq!(c.state(id), DraftState::Outdated);
        assert_eq!(stored(&store).len(), 1, "still a draft");
        c.as_file_comment(id);
        let left = stored(&store);
        assert_eq!(
            (left[0].kind, left[0].text.as_str()),
            (DraftKind::File, "(line 11) on the old code")
        );
        assert!(poster.attempts.lock().unwrap().is_empty());
    }

    #[test]
    fn draft_from_older_head_blocked_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.sqlite");
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let id = {
            let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
            let mut old = composer_at(store, &poster, OLD);
            write(&mut old, inline("a.rs", Side::New, 11), "before the push")
        };
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert_eq!(c.state(id), DraftState::Outdated);
        // discarding is the other way out
        c.delete_draft(id);
        assert!(stored(&store).is_empty());
    }

    #[test]
    fn stored_draft_with_empty_or_short_source_commit_is_blocked() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let empty = raw_inline(&store, "", "no head");
        let short = raw_inline(&store, &HEAD[..6], "six chars");
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert!(poster.attempts.lock().unwrap().is_empty(), "not trusted");
        assert_eq!(c.state(empty), DraftState::Outdated);
        assert_eq!(c.state(short), DraftState::Outdated);
    }

    #[test]
    fn abbreviated_source_commit_still_matches() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        raw_inline(&store, &HEAD[..12], "short draft head");
        let mut c = composer(store.clone(), &poster);
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        // and a full draft head against an abbreviated screen head
        raw_inline(&store, HEAD, "full draft head");
        let mut short_screen = composer_at(store.clone(), &poster, &HEAD[..12]);
        submit(&mut short_screen, &mut comments);
        assert_eq!(
            texts(&poster.posted.lock().unwrap()),
            ["short draft head", "full draft head"]
        );
        assert!(stored(&store).is_empty());
    }

    #[test]
    fn second_batch_skips_drafts_in_flight_across_composers() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        let mut a = composer(store.clone(), &poster);
        a.share_in_flight(reg.clone());
        let one = write(&mut a, Target::Pr, "one");
        let two = write(&mut a, Target::Pr, "two");
        *poster.hold.lock().unwrap() = Some("one".into());
        a.request_submit();
        assert!(a.confirm_submit());
        until("first post", || poster.attempts.lock().unwrap().len() == 1);
        let mut b = composer(store.clone(), &poster);
        b.share_in_flight(reg.clone());
        assert_eq!(
            b.state(one),
            DraftState::Posting,
            "shown as posting elsewhere"
        );
        assert_eq!(b.postable(), [two], "never in a second batch");
        let mut cb = Comments::new();
        submit(&mut b, &mut cb);
        poster.release();
        let mut ca = Comments::new();
        wait(&mut a, &mut ca);
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["two", "one"]);
        assert_eq!(poster.attempts.lock().unwrap().len(), 2);
        assert!(stored(&store).is_empty() && reg.lock().unwrap().is_empty());
    }

    #[test]
    fn back_during_batch_stops_after_current_draft_and_keeps_rest() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        // Stop button: the screen stays
        let mut a = composer(store.clone(), &poster);
        a.share_in_flight(reg.clone());
        for t in ["one", "two", "three"] {
            write(&mut a, Target::Pr, t);
        }
        *poster.hold.lock().unwrap() = Some("one".into());
        a.request_submit();
        assert!(a.confirm_submit());
        until("first post", || poster.attempts.lock().unwrap().len() == 1);
        a.cancel_batch();
        poster.release();
        let mut comments = Comments::new();
        wait(&mut a, &mut comments);
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["one"]);
        let left: Vec<String> = stored(&store).into_iter().map(|d| d.text).collect();
        assert_eq!(left, ["two", "three"]);
        assert_eq!(a.drafts.len(), 2);
        // Back: the screen (and its composer) goes away mid-post
        *poster.hold.lock().unwrap() = Some("two".into());
        a.request_submit();
        assert!(a.confirm_submit());
        until("second post", || poster.attempts.lock().unwrap().len() == 2);
        drop(a);
        poster.release();
        until("worker done", || {
            reg.lock().unwrap().is_empty() && stored(&store).len() == 1
        });
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "two"]);
        assert_eq!(stored(&store)[0].text, "three", "the rest stays a draft");
    }

    #[test]
    fn reload_during_batch_never_double_posts() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        let mut a = composer(store.clone(), &poster);
        a.share_in_flight(reg.clone());
        let one = write(&mut a, Target::Pr, "one");
        let two = write(&mut a, Target::Pr, "two");
        *poster.hold.lock().unwrap() = Some("one".into());
        a.request_submit();
        assert!(a.confirm_submit());
        until("first post", || poster.attempts.lock().unwrap().len() == 1);
        drop(a); // Reload
        let mut b = composer(store.clone(), &poster);
        b.share_in_flight(reg.clone());
        assert_eq!(b.state(one), DraftState::Posting);
        b.request_submit();
        assert_eq!(b.confirm.as_deref(), Some(&[two][..]));
        assert!(b.confirm_submit());
        poster.release();
        let mut comments = Comments::new();
        wait(&mut b, &mut comments);
        until("old worker done", || reg.lock().unwrap().is_empty());
        let mut posted = texts(&poster.posted.lock().unwrap());
        posted.sort();
        assert_eq!(posted, ["one", "two"], "each exactly once");
        assert!(stored(&store).is_empty());
        b.request_submit();
        assert!(b.confirm.is_none(), "nothing left to post");
        assert_eq!(poster.attempts.lock().unwrap().len(), 2);
    }

    #[test]
    fn posted_but_not_removed_is_never_posted_again_and_cleaned_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.sqlite");
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let id = {
            let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
            // the batch's delete and its retry fail, and so does the cleanup on the next submit
            let mut c = Composer::with_failing_deletes(repo(), 7, store.clone(), 3);
            c.head = HEAD.into();
            c.connect(poster.clone(), source(HEAD), None);
            let id = write(&mut c, Target::Pr, "once");
            let mut comments = Comments::new();
            submit(&mut c, &mut comments);
            assert_eq!(c.state(id), DraftState::PostedNotRemoved);
            assert_eq!(stored(&store).len(), 1, "the delete failed");
            assert_eq!(
                store.lock().unwrap().posted_draft_ids(&repo(), 7).unwrap(),
                [id]
            );
            c.request_submit();
            assert!(c.confirm.is_none(), "never offered again");
            id
        };
        // restart, and the cleanup delete fails once more: still not postable
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut c = Composer::with_failing_deletes(repo(), 7, store.clone(), 1);
        c.connect(poster.clone(), source(HEAD), None);
        assert_eq!(c.state(id), DraftState::PostedNotRemoved);
        assert!(c.postable().is_empty());
        drop(c);
        // the next open cleans the tombstone up
        let c = composer(store.clone(), &poster);
        assert!(c.drafts.is_empty() && stored(&store).is_empty());
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["once"]);
    }

    #[test]
    fn draft_deleted_elsewhere_before_its_turn_is_skipped_as_gone() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut a = composer(store.clone(), &poster);
        write(&mut a, Target::Pr, "one");
        let two = write(&mut a, Target::Pr, "two");
        a.request_submit();
        assert_eq!(a.confirm.as_ref().map(Vec::len), Some(2));
        let mut b = composer(store.clone(), &poster);
        b.delete_draft(two);
        assert!(a.confirm_submit());
        let mut comments = Comments::new();
        wait(&mut a, &mut comments);
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["one"]);
        assert!(a.drafts.is_empty(), "the gone draft left the list");
    }

    #[test]
    fn rows_of_another_review_abort_the_batch() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        // the service moved on (another head) since this screen loaded
        c.connect(poster.clone(), source(OLD), None);
        let line = write(&mut c, inline("a.rs", Side::New, 11), "x");
        write(&mut c, Target::Pr, "after it");
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert!(matches!(c.state(line), DraftState::Failed(e) if e.contains("reloaded")));
        // or another pull request altogether
        let other: RowSource = Arc::new(|_| {
            Ok(FileRows {
                repo: repo(),
                pr_id: 8,
                head: HEAD.into(),
                merge_base: String::new(),
                rows: Arc::new(vec![row(RowKind::Add, None, Some(11))]),
                binary: false,
            })
        });
        c.connect(poster.clone(), other, None);
        submit(&mut c, &mut comments);
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert!(matches!(c.state(line), DraftState::Failed(_)));
    }

    #[test]
    fn undo_targets_the_toast_state_not_the_live_thread() {
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        let mut comments = Comments::new();
        comments.set(build_threads(vec![comment(1, false)]), 0);
        c.toggle_resolved(&mut comments, &1.into());
        wait(&mut c, &mut comments);
        // a refresh meanwhile shows it unresolved again (someone else)
        comments.set(build_threads(vec![comment(1, false)]), 0);
        c.undo(&mut comments);
        wait(&mut c, &mut comments);
        assert_eq!(
            *poster.resolves.lock().unwrap(),
            [(ThreadId::from(1), true), (ThreadId::from(1), false)]
        );
        assert!(!comments.threads[0].root.resolved);
    }

    #[test]
    fn refresh_during_resolve_keeps_the_optimistic_state() {
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        let mut comments = Comments::new();
        comments.set(build_threads(vec![comment(1, false)]), 0);
        *poster.hold_resolve.lock().unwrap() = true;
        c.toggle_resolved(&mut comments, &1.into());
        comments.set(build_threads(vec![comment(1, false)]), 0);
        c.reapply_pending(&mut comments);
        assert!(
            comments.threads[0].root.resolved,
            "in flight: the click wins"
        );
        poster.release();
        wait(&mut c, &mut comments);
        assert!(comments.threads[0].root.resolved);
    }

    // ---- re-review: unrecorded posts, counts, registry on open ----

    #[test]
    fn unrecorded_post_is_never_submitted_again_in_this_process() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        // the server accepts, then neither the tombstone nor the delete can be written
        let mut c = Composer::with_failing_store(repo(), 7, store.clone(), reg.clone(), 99, 99);
        c.head = HEAD.into();
        c.connect(poster.clone(), source(HEAD), None);
        let id = write(&mut c, Target::Pr, "once");
        let mut comments = Comments::new();
        submit(&mut c, &mut comments);
        assert_eq!(c.state(id), DraftState::PostedNotRemoved);
        let err = c.error.clone().unwrap_or_default();
        assert!(err.contains("do not submit it again"), "{err}");
        c.request_submit();
        assert!(c.confirm.is_none());
        // another screen of this process does not offer it either
        let mut b = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
        b.connect(poster.clone(), source(HEAD), None);
        assert!(b.postable().is_empty());
        assert_eq!(b.state(id), DraftState::Posting);
        b.request_submit();
        assert!(b.confirm.is_none());
        assert_eq!(
            poster.attempts.lock().unwrap().len(),
            1,
            "posted exactly once"
        );
    }

    #[test]
    fn progress_counts_only_drafts_that_will_post() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        let mut c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
        c.head = HEAD.into();
        c.connect(poster.clone(), source(HEAD), None);
        let blocked = write(&mut c, inline("a.rs", Side::New, 500), "blocked");
        let outdated = write(&mut c, inline("a.rs", Side::New, 11), "outdated");
        let one = write(&mut c, Target::Pr, "one");
        let two = write(&mut c, Target::Pr, "two");
        let three = write(&mut c, Target::Pr, "three");
        c.mark_blocked(blocked, InlineBlock::LineNotInDiff);
        c.states.insert(outdated, DraftState::Outdated);
        assert_eq!(
            c.postable(),
            [one, two, three],
            "blocked ones are not offered"
        );
        poster.hold_text(Some("one"));
        c.request_submit();
        assert_eq!(c.confirm.as_deref(), Some(&[one, two, three][..]));
        assert!(c.confirm_submit());
        let mut comments = Comments::new();
        until("first post", || poster.attempts.lock().unwrap().len() == 1);
        c.poll(&mut comments);
        assert_eq!(c.progress().as_deref(), Some("Posting 1 of 3..."));
        // another batch claims `two` meanwhile: it no longer counts here
        reg.lock().unwrap().insert(two);
        poster.hold_text(Some("three"));
        until("third post", || poster.attempts.lock().unwrap().len() == 2);
        std::thread::sleep(Duration::from_millis(50));
        c.poll(&mut comments);
        assert_eq!(c.progress().as_deref(), Some("Posting 2 of 2..."));
        poster.release();
        wait(&mut c, &mut comments);
        reg.lock().unwrap().remove(&two);
        assert_eq!(texts(&poster.posted.lock().unwrap()), ["one", "three"]);
        assert_eq!(
            c.state(blocked),
            DraftState::Blocked(InlineBlock::LineNotInDiff)
        );
    }

    #[test]
    fn cleanup_on_first_open_respects_the_in_flight_registry() {
        let store = mem_store();
        let id = raw_inline(&store, HEAD, "being finished elsewhere");
        store.lock().unwrap().mark_draft_posted(id).unwrap();
        let reg = InFlight::default();
        reg.lock().unwrap().insert(id);
        let c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
        assert_eq!(
            stored(&store).len(),
            1,
            "its worker deletes it, not this open"
        );
        assert!(c.postable().is_empty());
        reg.lock().unwrap().clear();
        let c = Composer::with_in_flight(repo(), 7, store.clone(), reg);
        assert!(c.drafts.is_empty() && stored(&store).is_empty());
    }

    #[test]
    fn drafts_in_flight_cannot_be_edited_deleted_or_converted() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let reg = InFlight::default();
        let mut c = Composer::with_in_flight(repo(), 7, store.clone(), reg.clone());
        c.head = HEAD.into();
        c.connect(poster.clone(), source(HEAD), None);
        let id = write(&mut c, inline("a.rs", Side::New, 11), "x");
        reg.lock().unwrap().insert(id);
        c.edit_draft(id);
        assert!(c.editor.is_none());
        c.delete_draft(id);
        c.as_file_comment(id);
        let left = stored(&store);
        assert_eq!((left.len(), left[0].kind), (1, DraftKind::Inline));
        assert!(c.error.is_some());
    }

    #[test]
    fn stop_shows_stopping_until_the_batch_ends() {
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        write(&mut c, Target::Pr, "one");
        write(&mut c, Target::Pr, "two");
        poster.hold_text(Some("one"));
        c.request_submit();
        assert!(c.confirm_submit());
        until("first post", || poster.attempts.lock().unwrap().len() == 1);
        assert!(!c.stopping());
        c.cancel_batch();
        assert!(c.stopping());
        poster.release();
        let mut comments = Comments::new();
        wait(&mut c, &mut comments);
        assert!(!c.stopping());
        assert_eq!(stored(&store).len(), 1);
    }

    fn range(path: &str, start: (Side, u32), side: Side, line: u32) -> Target {
        Target::Inline {
            path: path.into(),
            side,
            line,
            start: Some(start),
        }
    }

    fn me() -> UserRef {
        UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        }
    }

    /// A draft on a line range keeps its first line and side across a restart and is posted
    /// as that range, never as its last line alone; a forge without ranges refuses one.
    #[test]
    fn multi_line_draft_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.sqlite");
        let poster = Arc::new(MemoryPoster::new(HEAD));
        // a.rs: from the deleted old line 11 to the context line 12 (new side)
        let target = range("a.rs", (Side::Old, 11), Side::New, 12);
        {
            let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
            let mut c = composer(store, &poster);
            c.forge = Forge::GitHub;
            write(&mut c, target.clone(), "these lines");
        }
        let store = Arc::new(Mutex::new(Store::open(&file).unwrap()));
        let mut c = composer(store.clone(), &poster);
        c.forge = Forge::GitHub;
        let d = c.drafts[0].clone();
        assert_eq!(d.start, Some((Side::Old, 11)));
        assert_eq!(Target::of(&d), Some(target.clone()));
        assert_eq!(describe(&d), "a.rs:11 (old side) to 12 (new side)");
        submit(&mut c, &mut Comments::new());
        assert_eq!(
            *poster.posted.lock().unwrap(),
            [NewComment::Inline {
                path: "a.rs".into(),
                side: Side::New,
                line: 12,
                start: Some((Side::Old, 11)),
                text: "these lines".into(),
            }]
        );
        assert!(stored(&store).is_empty());
        // Bitbucket takes no ranges: no composer opens on one
        let mut bb = composer(mem_store(), &poster);
        bb.open_editor(target);
        assert!(bb.editor.is_none() && bb.error.is_some());
    }

    /// GitLab saves a body without carriage returns and trailing blanks: "Check comments"
    /// normalises both sides the same way before comparing.
    #[test]
    fn looks_posted_matches_gitlab_normalized_body() {
        assert_eq!(
            normalize_for_match(Forge::GitLab, "a\r\n  b \t\r\nc\rd\n\n"),
            "a\n  b\ncd"
        );
        assert_eq!(normalize_for_match(Forge::GitHub, "a  \nb\n\n"), "a\nb");
        assert_ne!(
            normalize_for_match(Forge::GitHub, "c\rd"),
            "cd",
            "only GitLab drops a lone carriage return"
        );
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        c.forge = Forge::GitLab;
        let mut comments = Comments::new();
        let id = write(&mut c, Target::Pr, "first  \r\nsecond\rhalf\t\r\n");
        poster
            .script
            .lock()
            .unwrap()
            .push_back(Some(PostError::Unconfirmed));
        submit(&mut c, &mut comments);
        let mut mine = comment(80, false);
        mine.author = me();
        mine.body = "first\nsecondhalf".into();
        assert!(c.check_comments(id));
        comments.set(build_threads(vec![mine]), 0);
        c.comments_checked(&comments, Some(&me()));
        assert_eq!(
            c.state(id),
            DraftState::Unconfirmed {
                checked: true,
                looks_posted: true
            }
        );
        assert!(c.postable().is_empty(), "a hint only");
    }

    /// New-side rows 10 to 12 of a.rs with their text.
    fn texts_rows() -> Vec<Row> {
        let r = |kind, old, new, text: &str| Row {
            kind,
            old_line: old,
            new_line: new,
            text: text.into(),
        };
        vec![
            r(RowKind::HunkHeader, None, None, "@@"),
            r(RowKind::Context, Some(10), Some(10), "ten"),
            r(RowKind::Del, Some(11), None, "gone"),
            r(RowKind::Add, None, Some(11), "eleven"),
            r(RowKind::Context, Some(12), Some(12), "twelve"),
        ]
    }

    #[test]
    fn github_suggestion_block_format() {
        let lines = ["let a = 1;".to_string(), "let b = 2;".to_string()];
        assert_eq!(
            suggestion_block(SuggestionSyntax::GitHubFence, &lines, 12, (11, 12)),
            "```suggestion\nlet a = 1;\nlet b = 2;\n```"
        );
        // an empty suggestion removes the lines
        assert_eq!(
            suggestion_block(SuggestionSyntax::GitHubFence, &[], 12, (12, 12)),
            "```suggestion\n```"
        );
        // "Suggest change" on new lines 11-12 appends them to the text
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        c.forge = Forge::GitHub;
        c.open_editor(range("a.rs", (Side::New, 11), Side::New, 12));
        assert_eq!(
            c.editor.as_ref().unwrap().suggestions,
            Some(SuggestionSyntax::GitHubFence)
        );
        c.editor.as_mut().unwrap().text = "maybe:".into();
        c.insert_suggestion(&texts_rows());
        assert_eq!(
            c.editor.as_ref().unwrap().text,
            "maybe:\n```suggestion\neleven\ntwelve\n```"
        );
    }

    #[test]
    fn gitlab_suggestion_offsets_relative_to_anchor() {
        let lines: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        assert_eq!(
            suggestion_block(SuggestionSyntax::GitLabRange, &lines, 12, (10, 13)),
            "```suggestion:-2+1\na\nb\nc\nd\n```"
        );
        assert_eq!(
            suggestion_block(SuggestionSyntax::GitLabRange, &lines[..1], 12, (12, 12)),
            "```suggestion:-0+0\na\n```"
        );
        // the comment sits on its last line (12): the range starts one line above it
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(mem_store(), &poster);
        c.forge = Forge::GitLab;
        // GitLab's capabilities (ranges, suggestions) arrive with its provider: the composer
        // is set up as they will be
        c.editor = Some(Editor {
            target: range("a.rs", (Side::New, 11), Side::New, 12),
            text: String::new(),
            draft_id: None,
            comment_type: None,
            focus: false,
            suggestions: Some(SuggestionSyntax::GitLabRange),
        });
        c.insert_suggestion(&texts_rows());
        assert_eq!(
            c.editor.as_ref().unwrap().text,
            "```suggestion:-1+0\neleven\ntwelve\n```"
        );
        // Bitbucket has no suggestions: nothing is inserted
        let mut bb = composer(mem_store(), &poster);
        bb.open_editor(inline("a.rs", Side::New, 12));
        assert_eq!(bb.editor.as_ref().unwrap().suggestions, None);
        bb.insert_suggestion(&texts_rows());
        assert_eq!(bb.editor.as_ref().unwrap().text, "");
    }

    /// Decision D7: on GitLab a line starting with "/" and a letter would run a quick action.
    #[test]
    fn gitlab_slash_command_line_refused() {
        assert!(quick_action_guard(Forge::GitLab, "/approve").is_err());
        assert!(quick_action_guard(Forge::GitLab, "looks good\n/assign @me").is_err());
        assert!(quick_action_guard(Forge::GitLab, "  /merge").is_err());
        assert!(quick_action_guard(Forge::GitLab, "a/b and / alone\n/ x\n//c\n/1").is_ok());
        for f in [Forge::BitbucketCloud, Forge::GitHub] {
            assert!(quick_action_guard(f, "/approve").is_ok());
        }
        let store = mem_store();
        let poster = Arc::new(MemoryPoster::new(HEAD));
        let mut c = composer(store.clone(), &poster);
        c.forge = Forge::GitLab;
        // "Save draft" refuses it and keeps the text in the composer
        c.open_editor(Target::Pr);
        c.editor.as_mut().unwrap().text = "ok\n/merge".into();
        assert!(!c.save_editor());
        assert!(c.error.as_deref().unwrap().contains("quick action"));
        assert_eq!(c.editor.as_ref().unwrap().text, "ok\n/merge");
        assert!(stored(&store).is_empty());
        // switching composers keeps it as a draft, but a submit never sends it
        c.open_editor(Target::File("a.rs".into()));
        let id = stored(&store)[0].id;
        submit(&mut c, &mut Comments::new());
        assert!(poster.attempts.lock().unwrap().is_empty());
        assert!(matches!(c.state(id), DraftState::Failed(_)));
        assert_eq!(stored(&store).len(), 1, "kept");
    }
}
