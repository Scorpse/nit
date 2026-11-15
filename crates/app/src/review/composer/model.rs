//! Draft targets, poster seam, error texts and the per-draft state shown to the user.

use super::*;
use crate::theme::{AMBER, RED};

/// Bitbucket inline posting. Verified live against a real pull request: a comment with
/// `inline.to` lands on that line of the new file (added and context lines), one with
/// `inline.from` on that line of the old file (deleted lines), and Bitbucket echoes the same
/// anchor back. Bitbucket has no multi-line comments, so ranges are refused by the provider.
pub const INLINE_ENABLED_BITBUCKET: bool = true;

/// Whether inline drafts may be posted on `forge`: each forge has its own compile-time gate
/// (`INLINE_ENABLED_BITBUCKET`, `INLINE_ENABLED_GITHUB`, `INLINE_ENABLED_GITLAB`), flipped
/// only after that forge's anchoring was verified.
pub fn inline_posting_enabled_for(forge: Forge) -> bool {
    match forge {
        Forge::GitHub => providers::github::INLINE_ENABLED_GITHUB,
        Forge::GitLab => providers::gitlab::INLINE_ENABLED_GITLAB,
        Forge::BitbucketCloud => INLINE_ENABLED_BITBUCKET,
    }
}

/// The refusal shown on Submit for `n` inline drafts while inline posting is disabled.
pub(super) fn inline_disabled_message(n: usize) -> String {
    format!(
        "{n} inline draft(s) not posted: inline comments are disabled until their placement \
         is verified. They stay saved. Use \"Post as file comment instead\" or \"Post as PR \
         comment instead\" on a draft to post it another way."
    )
}

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

    pub(super) fn fill(&self, d: &mut Draft) {
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
pub(super) fn new_comment(d: &Draft) -> Option<NewComment> {
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

pub(super) fn side_word(s: Side) -> &'static str {
    match s {
        Side::New => "new",
        Side::Old => "old",
    }
}

/// "src/a.rs:10-12 (new side)" or "src/a.rs:9 (old side) to 12 (new side)" for a range,
/// "12 (new side)" for one line.
pub(super) fn lines_word(side: Side, line: u32, start: Option<(Side, u32)>) -> String {
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
pub(super) const SUGGESTION_MAX_OFFSET: u32 = 100;

/// The comment sits where the draft would have been posted.
pub(super) fn same_place(c: &Comment, d: &Draft) -> bool {
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
pub(super) fn head_for_check(server: &str, local: &str) -> String {
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
