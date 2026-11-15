use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::diff::{Row, RowKind};
use crate::draft::Side;
use crate::model::{PrState, PullRequest, ReviewState, UserRef};
use crate::stacks::Stack;

/// Opaque, provider-made comment id, unique within one PR. Bitbucket "123"; GitHub
/// "rc:123" / "ic:123"; GitLab "note:123".
///
/// Ordered numerically when both ids are plain numbers (Bitbucket keeps its old order),
/// numbers before other ids, other ids as strings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommentId(pub String);

/// What replies and resolve/unresolve address. Bitbucket: the root comment id; GitHub:
/// the review thread node id; GitLab: the discussion id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

impl CommentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id as a number, for forges whose ids are numbers (Bitbucket).
    pub fn as_u64(&self) -> Option<u64> {
        self.0.parse().ok()
    }

    fn order_key(&self) -> (Option<u64>, &str) {
        (self.as_u64(), &self.0)
    }
}

impl Ord for CommentId {
    fn cmp(&self, other: &Self) -> Ordering {
        let (a, b) = (self.order_key(), other.order_key());
        match (a.0, b.0) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.1.cmp(b.1)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.1.cmp(b.1),
        }
    }
}

impl PartialOrd for CommentId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<u64> for CommentId {
    fn from(n: u64) -> Self {
        CommentId(n.to_string())
    }
}

impl fmt::Display for CommentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ThreadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id as a number, for forges whose thread ids are comment numbers (Bitbucket).
    pub fn as_u64(&self) -> Option<u64> {
        self.0.parse().ok()
    }
}

impl From<u64> for ThreadId {
    fn from(n: u64) -> Self {
        ThreadId(n.to_string())
    }
}

impl From<&CommentId> for ThreadId {
    fn from(c: &CommentId) -> Self {
        ThreadId(c.0.clone())
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
/// Both `from` and `to` being `None` means a file-level comment.
pub struct Anchor {
    pub path: String,
    /// The file's old path when the forge says it differs (renames, GitLab).
    pub old_path: Option<String>,
    /// Line in the old file. For a multi-line comment, its last line.
    pub from: Option<u32>,
    /// Line in the new file. For a multi-line comment, its last line.
    pub to: Option<u32>,
    /// First line of a multi-line comment and its side; `None` for a single line.
    /// Display only on Bitbucket (where it is on the anchor line's side).
    pub start: Option<(Side, u32)>,
    /// The head commit the comment was made on, when the forge says.
    pub commit: Option<String>,
    /// Bitbucket's `inline.outdated`: the anchored line changed after the comment was
    /// made, so `from`/`to` may point at other content in the current diff.
    pub outdated: bool,
}

impl Anchor {
    /// The first line of a multi-line comment, whatever its side.
    pub fn start_line(&self) -> Option<u32> {
        self.start.map(|(_, l)| l)
    }
}

/// Why a thread anchored on a file is shown as outdated instead of on a diff line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutdatedReason {
    /// Its file is not in the current diff (removed, or its changes reverted, by a later push).
    FileNotInDiff,
    /// It is on the old path of a file this diff renames.
    OldPath,
    /// Bitbucket marks it outdated: its line changed after the comment was made.
    Flagged,
    /// Its line is not shown in the current diff (or the file is binary).
    LineNotInDiff,
}

/// `author.id` is the forge's user id (on Bitbucket the account uuid, else its Atlassian
/// `account_id`), else empty (bots and apps may have neither). `author.display_name` is
/// never empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comment {
    pub id: CommentId,
    /// What a reply to (or resolving) this comment's thread addresses; None: the comment
    /// cannot be replied to or resolved (GitHub issue comments). Parentless comments that
    /// share a thread form one thread.
    pub thread: Option<ThreadId>,
    /// Bitbucket's reply link; other forges group by `thread`.
    pub parent_id: Option<CommentId>,
    /// Whether this user may resolve/unresolve the thread (per-thread forge permission).
    pub can_resolve: bool,
    pub author: UserRef,
    pub body: String,
    pub created_on: DateTime<Utc>,
    pub updated_on: DateTime<Utc>,
    pub deleted: bool,
    pub resolved: bool,
    pub anchor: Option<Anchor>,
    /// Posted by an app, integration or bot account (see [`looks_like_bot_name`]).
    pub author_is_bot: bool,
}

/// Display name used when an author has no usable name.
pub const UNKNOWN_AUTHOR: &str = "Unknown author";

/// Whole words that mark an automated author (case-insensitive).
const BOT_WORDS: [&str; 18] = [
    "bot",
    "bots",
    "rovo",
    "dependabot",
    "renovate",
    "renovatebot",
    "snyk",
    "sonar",
    "sonarcloud",
    "sonarqube",
    "sonarlint",
    "coderabbit",
    "coderabbitai",
    "codacy",
    "copilot",
    "claude",
    "gpt",
    "chatgpt",
];

/// Whether a display name or nickname looks like an automated review tool or bot: a known
/// tool name as a whole word, "Bitbucket Pipelines", a `gpt…` word (`gpt4o`), or a
/// separated `bot` word or suffix (`renovate-bot`, `app[bot]`). A `bot` glued inside a word
/// (`Talbot`) does not count. A heuristic: a person named "Claude" is flagged too.
pub fn looks_like_bot_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.windows(2).any(|w| w == ["bitbucket", "pipelines"])
        || words
            .iter()
            .any(|w| BOT_WORDS.contains(w) || w.starts_with("gpt"))
}

/// Open or resolved state of a PR task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Open,
    Resolved,
}

/// A read-only PR task, optionally attached to a comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrTask {
    pub id: u64,
    pub state: TaskState,
    pub content: String,
    pub creator: UserRef,
    pub comment_id: Option<CommentId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub root: Comment,
    pub replies: Vec<Comment>,
}

impl Thread {
    /// What a reply or resolve addresses; None when the thread takes neither.
    pub fn id(&self) -> Option<&ThreadId> {
        self.root.thread.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewComment {
    Pr {
        text: String,
    },
    File {
        path: String,
        text: String,
    },
    Inline {
        path: String,
        side: Side,
        line: u32,
        /// First line of a multi-line comment; None for one line (always None on Bitbucket).
        start: Option<(Side, u32)>,
        text: String,
    },
    Reply {
        thread: ThreadId,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    Approve,
    RequestChanges,
    ClearApproval,
    ClearRequestChanges,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineBlock {
    HeadMismatch,
    /// The server's diff base (merge base, or destination branch) is not the one reviewed.
    BaseMismatch,
    BinaryFile,
    LineNotInDiff,
}

/// How a forge pins an inline comment to a revision (spec 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorModel {
    /// Bitbucket: anchors to "the current PR diff"; only the head is compared.
    CurrentDiff,
    /// GitHub: `commit_id` = head; the destination branch must also be unchanged.
    HeadCommit,
    /// GitLab: `position` with base/head SHAs; head and merge base must both match.
    DiffRefs,
}

/// What the server says the PR is right now; compared with the local fetch before an
/// inline post. Empty strings mean unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerRevision {
    pub head: String,
    /// The merge base the server diffs against (GitLab `base_sha`); None when not reported.
    pub base: Option<String>,
    /// GitLab `start_sha` (the target branch commit); None elsewhere.
    pub start: Option<String>,
    pub dest_branch: String,
}

/// The revision the user reviewed (the local fetch): head and merge base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostRevision {
    pub head: String,
    pub merge_base: String,
}

/// Groups comments into threads. Total: never panics or loops.
/// Comments with a `parent_id` (Bitbucket) are walked to their top-most existing
/// ancestor: orphans (missing parent) and members of parent cycles become roots, nested
/// replies are flattened. Parentless comments sharing a `thread` (GitHub, GitLab) form one
/// thread whose root is the earliest of them. Roots and replies are ordered by
/// `created_on` then id. Deleted roots with replies are kept.
pub fn build_threads(comments: Vec<Comment>) -> Vec<Thread> {
    let by_id: HashMap<&CommentId, &Comment> = comments.iter().map(|c| (&c.id, c)).collect();
    let key = |c: &Comment| (c.created_on, c.id.clone());

    // The root of each `thread` group: its earliest parentless member.
    let mut thread_root: HashMap<&ThreadId, &Comment> = HashMap::new();
    for c in comments.iter().filter(|c| c.parent_id.is_none()) {
        if let Some(t) = &c.thread {
            let e = thread_root.entry(t).or_insert(c);
            if key(c) < key(e) {
                *e = c;
            }
        }
    }
    let group_root = |c: &Comment| -> CommentId {
        match (&c.parent_id, &c.thread) {
            (None, Some(t)) => thread_root[t].id.clone(),
            _ => c.id.clone(),
        }
    };

    // Memoised root resolution, linear overall. A comment whose ancestor chain
    // reaches a cycle ("tainted") starts its own thread.
    enum State {
        Visiting,
        Root(CommentId),
        Tainted,
    }
    let mut state: HashMap<&CommentId, State> = HashMap::new();
    for c in &comments {
        if state.contains_key(&c.id) {
            continue;
        }
        let mut path: Vec<&CommentId> = Vec::new();
        let mut cur = &c.id;
        // Outcome for the whole path: Some(root) or None when tainted.
        let outcome: Option<CommentId> = loop {
            state.insert(cur, State::Visiting);
            path.push(cur);
            let parent = by_id[cur].parent_id.as_ref().and_then(|p| by_id.get(p));
            let Some(parent) = parent else {
                // no parent or orphan: `cur` starts (or, by thread, joins) a thread
                break Some(group_root(by_id[cur]));
            };
            match state.get(&parent.id) {
                Some(State::Visiting) | Some(State::Tainted) => break None,
                Some(State::Root(r)) => break Some(r.clone()),
                None => cur = &parent.id,
            }
        };
        for id in path {
            state.insert(
                id,
                match &outcome {
                    Some(r) => State::Root(r.clone()),
                    None => State::Tainted,
                },
            );
        }
    }
    let root_of = |c: &Comment| match &state[&c.id] {
        State::Root(r) => r.clone(),
        _ => c.id.clone(),
    };

    let mut threads: Vec<Thread> = Vec::new();
    let mut index: HashMap<CommentId, usize> = HashMap::new();
    let mut replies: Vec<(CommentId, Comment)> = Vec::new();
    for c in &comments {
        let root = root_of(c);
        if root == c.id {
            index.insert(c.id.clone(), threads.len());
            threads.push(Thread {
                root: c.clone(),
                replies: Vec::new(),
            });
        } else {
            replies.push((root, c.clone()));
        }
    }
    for (root, reply) in replies {
        if let Some(&i) = index.get(&root) {
            threads[i].replies.push(reply);
        }
    }
    for t in &mut threads {
        t.replies.sort_by_key(key);
    }
    threads.sort_by_key(|t| key(&t.root));
    threads
}

/// Whether an inline comment may be posted on `side`/`line` (and from `start`, for a
/// multi-line comment). Checks in order: head mismatch (or an empty commit), base mismatch
/// per anchor model, binary file, line (and start) not present in the diff rows.
///
/// `fetched_dest` is the destination branch the local diff was fetched against.
/// `CurrentDiff` compares heads only (Bitbucket, unchanged); `HeadCommit` also needs the
/// server's destination branch to be `fetched_dest`; `DiffRefs` also needs the server's
/// merge base to be the local one (an unknown server base blocks).
#[allow(clippy::too_many_arguments)]
pub fn inline_allowed(
    model: AnchorModel,
    server: &ServerRevision,
    local: &PostRevision,
    fetched_dest: &str,
    rows: &[Row],
    binary: bool,
    side: Side,
    line: u32,
    start: Option<(Side, u32)>,
) -> Result<(), InlineBlock> {
    if server.head.is_empty() || local.head.is_empty() || server.head != local.head {
        return Err(InlineBlock::HeadMismatch);
    }
    let base_ok = match model {
        AnchorModel::CurrentDiff => true,
        AnchorModel::HeadCommit => {
            !server.dest_branch.is_empty() && server.dest_branch == fetched_dest
        }
        AnchorModel::DiffRefs => {
            let base = server.base.as_deref().unwrap_or("");
            !base.is_empty() && base == local.merge_base
        }
    };
    if !base_ok {
        return Err(InlineBlock::BaseMismatch);
    }
    if binary {
        return Err(InlineBlock::BinaryFile);
    }
    let Some(end) = row_index(rows, side, line) else {
        return Err(InlineBlock::LineNotInDiff);
    };
    if let Some((s_side, s_line)) = start {
        // The start must be a code row before the end, inside the same hunk.
        let ok = row_index(rows, s_side, s_line).is_some_and(|s| {
            s <= end && !rows[s..end].iter().any(|r| r.kind == RowKind::HunkHeader)
        });
        if !ok {
            return Err(InlineBlock::LineNotInDiff);
        }
    }
    Ok(())
}

/// Index of the code row showing `line` on `side`.
fn row_index(rows: &[Row], side: Side, line: u32) -> Option<usize> {
    rows.iter().position(|r| match (side, r.kind) {
        (Side::New, RowKind::Context | RowKind::Add) => r.new_line == Some(line),
        (Side::Old, RowKind::Context | RowKind::Del) => r.old_line == Some(line),
        _ => false,
    })
}

/// Warnings when approving `id`: its stack parent is neither approved nor merged.
pub fn approval_warnings(stack: &Stack, prs: &HashMap<u64, PullRequest>, id: u64) -> Vec<String> {
    stack
        .edges
        .iter()
        .filter(|&&(_, child)| child == id)
        .filter_map(|&(parent, _)| prs.get(&parent))
        .filter(|p| {
            let approved = p.reviewers.iter().any(|r| r.state == ReviewState::Approved);
            p.state != PrState::Merged && !approved
        })
        .map(|p| {
            format!(
                "Parent PR #{} is not approved or merged yet; it should land first.",
                p.id
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
