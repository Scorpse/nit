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
mod tests {
    use super::*;
    use crate::model::{RepoRef, Reviewer};
    use chrono::TimeZone;

    fn user(n: &str) -> UserRef {
        UserRef {
            id: format!("{{{n}}}"),
            display_name: n.into(),
            login: None,
        }
    }

    /// A Bitbucket-shaped comment: numeric id, its own id as thread, parent link.
    fn c(id: u64, parent: Option<u64>, secs: i64) -> Comment {
        let t = Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap();
        Comment {
            id: id.into(),
            thread: Some(id.into()),
            parent_id: parent.map(CommentId::from),
            can_resolve: true,
            author: user("a"),
            body: format!("c{id}"),
            created_on: t,
            updated_on: t,
            deleted: false,
            resolved: false,
            anchor: None,
            author_is_bot: false,
        }
    }

    #[test]
    fn bot_detection_table() {
        let bots = [
            "Rovo Dev",
            "rovo",
            "Bitbucket Pipelines",
            "bitbucket-pipelines",
            "Dependabot",
            "dependabot[bot]",
            "Renovate Bot",
            "renovate-bot",
            "Snyk",
            "SonarCloud",
            "sonarqube-bot",
            "CodeRabbit",
            "coderabbitai",
            "Codacy Production",
            "GitHub Copilot",
            "Claude",
            "claude-code",
            "GPT-4 Reviewer",
            "gpt4o",
            "ChatGPT",
            "review_bot",
            "Deploy Bot",
            "ROVO DEV",
        ];
        for name in bots {
            assert!(looks_like_bot_name(name), "{name} should be a bot");
        }
        let people = [
            "Ada Lovelace",
            "Talbot Smith",
            "Abbott",
            "Robotnik",
            "Sonarika Rao",
            "Egypt Jones",
            "",
            "Unknown author",
            "Ana Souza",
        ];
        for name in people {
            assert!(!looks_like_bot_name(name), "{name} should not be a bot");
        }
    }

    #[test]
    fn reply_to_deleted_parent_becomes_root() {
        // Parent 77 was deleted and is not returned at all: its reply starts a thread.
        let t = build_threads(vec![c(1, None, 1), c(2, Some(77), 2)]);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].root.id, 2.into());
        // a reply to it addresses the orphan itself, as before
        assert_eq!(t[1].id(), Some(&ThreadId::from(2)));
        assert!(t[1].replies.is_empty());
        // Parent returned with `deleted: true`: the reply stays under it.
        let mut gone = c(77, None, 0);
        gone.deleted = true;
        let t = build_threads(vec![gone, c(2, Some(77), 2)]);
        assert_eq!(t.len(), 1);
        assert!(t[0].root.deleted);
        assert_eq!(ids(&t[0].replies), vec![2]);
    }

    fn ids(cs: &[Comment]) -> Vec<u64> {
        cs.iter().map(|c| c.id.as_u64().unwrap()).collect()
    }

    fn root_ids(t: &[Thread]) -> Vec<u64> {
        t.iter().map(|t| t.root.id.as_u64().unwrap()).collect()
    }

    /// A GitHub/GitLab-shaped comment: string id, grouped by `thread`, no parent link.
    fn g(id: &str, thread: Option<&str>, secs: i64) -> Comment {
        let mut x = c(0, None, secs);
        x.id = CommentId(id.into());
        x.thread = thread.map(|t| ThreadId(t.into()));
        x.parent_id = None;
        x
    }

    #[test]
    fn threads_group_by_thread_id_when_present() {
        let t = build_threads(vec![
            g("rc:3", Some("T1"), 30),
            g("rc:1", Some("T1"), 10),
            g("ic:9", None, 5),
            g("rc:2", Some("T2"), 20),
            g("rc:4", Some("T1"), 30),
        ]);
        let roots: Vec<&str> = t.iter().map(|t| t.root.id.as_str()).collect();
        assert_eq!(roots, ["ic:9", "rc:1", "rc:2"]);
        let r: Vec<&str> = t[1].replies.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(r, ["rc:3", "rc:4"]);
        assert_eq!(t[0].id(), None, "an issue comment takes no reply");
        assert_eq!(t[1].id(), Some(&ThreadId("T1".into())));
    }

    #[test]
    fn bitbucket_parent_walk_unchanged() {
        // Every Bitbucket comment carries its own id as thread; replies still follow
        // parent links, so a reply never joins a thread by its own id.
        let t = build_threads(vec![c(10, None, 1), c(2, Some(10), 2), c(3, Some(2), 3)]);
        assert_eq!(root_ids(&t), vec![10]);
        assert_eq!(ids(&t[0].replies), vec![2, 3]);
        assert_eq!(t[0].id(), Some(&ThreadId::from(10)));
    }

    #[test]
    fn numeric_ids_keep_numeric_order() {
        // "9" < "10" as numbers, not as strings: the Bitbucket tie-break is unchanged.
        let t = build_threads(vec![c(10, None, 1), c(9, None, 1)]);
        assert_eq!(root_ids(&t), vec![9, 10]);
        assert!(CommentId::from(2) < CommentId("ic:1".into()));
    }

    #[test]
    fn threads_group_replies_sorted_by_created_then_id() {
        let t = build_threads(vec![
            c(3, Some(1), 30),
            c(1, None, 10),
            c(2, Some(1), 30),
            c(4, None, 5),
        ]);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].root.id, 4.into());
        assert_eq!(t[1].root.id, 1.into());
        assert_eq!(ids(&t[1].replies), vec![2, 3]);
    }

    #[test]
    fn orphan_becomes_root() {
        let t = build_threads(vec![c(5, Some(99), 1)]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].root.id, 5.into());
        assert!(t[0].replies.is_empty());
    }

    #[test]
    fn nested_reply_flattened_under_root() {
        let t = build_threads(vec![c(1, None, 1), c(2, Some(1), 2), c(3, Some(2), 3)]);
        assert_eq!(t.len(), 1);
        assert_eq!(ids(&t[0].replies), vec![2, 3]);
    }

    #[test]
    fn deleted_root_with_replies_kept() {
        let mut root = c(1, None, 1);
        root.deleted = true;
        let t = build_threads(vec![root, c(2, Some(1), 2)]);
        assert_eq!(t.len(), 1);
        assert!(t[0].root.deleted);
        assert_eq!(ids(&t[0].replies), vec![2]);
    }

    #[test]
    fn cyclic_parents_terminate() {
        let t = build_threads(vec![c(1, Some(2), 1), c(2, Some(1), 2), c(3, Some(3), 3)]);
        let mut roots: Vec<u64> = root_ids(&t);
        roots.sort();
        assert_eq!(roots, vec![1, 2, 3]);
    }

    fn total(t: &[Thread]) -> usize {
        t.iter().map(|t| 1 + t.replies.len()).sum()
    }

    #[test]
    fn long_parent_chain_is_fast() {
        let n = 100_000u64;
        let input: Vec<Comment> = (1..=n)
            .map(|i| c(i, if i == 1 { None } else { Some(i - 1) }, i as i64))
            .collect();
        let start = std::time::Instant::now();
        let t = build_threads(input);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].replies.len(), (n - 1) as usize);
    }

    #[test]
    fn every_comment_lands_in_exactly_one_thread() {
        let input = vec![
            c(1, Some(2), 1),
            c(2, Some(1), 2),
            c(3, Some(3), 3),
            c(4, Some(1), 4), // leads into a cycle
            c(5, Some(4), 5), // leads into a cycle via 4
            c(6, None, 6),
            c(7, Some(6), 7),
            c(8, Some(99), 8),
        ];
        let n = input.len();
        let t = build_threads(input);
        assert_eq!(total(&t), n);
        let mut seen: Vec<u64> = t
            .iter()
            .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
            .map(|c| c.id.as_u64().unwrap())
            .collect();
        seen.sort();
        assert_eq!(seen, (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn duplicate_ids_do_not_panic() {
        let input = vec![
            c(1, None, 1),
            c(1, None, 2),
            c(2, Some(1), 3),
            c(2, Some(2), 4),
        ];
        let n = input.len();
        let t = build_threads(input);
        assert_eq!(total(&t), n);
    }

    #[test]
    fn empty_input_gives_no_threads() {
        assert!(build_threads(vec![]).is_empty());
    }

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>) -> Row {
        Row {
            kind,
            old_line: old,
            new_line: new,
            text: String::new(),
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            // Non-code rows must never validate, even with bogus line numbers.
            row(RowKind::HunkHeader, Some(40), Some(41)),
            row(RowKind::NoNewline, Some(42), Some(43)),
            row(RowKind::Context, Some(10), Some(10)),
            row(RowKind::Del, Some(11), None),
            row(RowKind::Add, None, Some(11)),
            row(RowKind::Context, Some(12), Some(15)),
        ]
    }

    /// The pre-multi-forge rule, `inline_allowed(server_head, local_head, rows, binary,
    /// side, line)`, which every Bitbucket (`CurrentDiff`) check must still follow.
    fn old_rule(
        server_head: &str,
        local_head: &str,
        rows: &[Row],
        binary: bool,
        side: Side,
        line: u32,
    ) -> Result<(), InlineBlock> {
        let server = ServerRevision {
            head: server_head.into(),
            // whatever the server says about the base, Bitbucket ignores it
            base: Some("unrelated".into()),
            start: None,
            dest_branch: "elsewhere".into(),
        };
        let local = PostRevision {
            head: local_head.into(),
            merge_base: "mb".into(),
        };
        inline_allowed(
            AnchorModel::CurrentDiff,
            &server,
            &local,
            "main",
            rows,
            binary,
            side,
            line,
            None,
        )
    }

    fn revs(head: &str, base: Option<&str>, dest: &str) -> (ServerRevision, PostRevision) {
        let server = ServerRevision {
            head: head.into(),
            base: base.map(Into::into),
            start: None,
            dest_branch: dest.into(),
        };
        let local = PostRevision {
            head: "h".into(),
            merge_base: "mb".into(),
        };
        (server, local)
    }

    fn check(model: AnchorModel, s: &ServerRevision, l: &PostRevision) -> Result<(), InlineBlock> {
        inline_allowed(model, s, l, "main", &rows(), false, Side::New, 11, None)
    }

    #[test]
    fn current_diff_model_matches_old_behaviour() {
        let table = [
            (
                "a",
                "b",
                false,
                Side::New,
                11,
                Err(InlineBlock::HeadMismatch),
            ),
            ("", "", false, Side::New, 11, Err(InlineBlock::HeadMismatch)),
            ("a", "a", true, Side::New, 999, Err(InlineBlock::BinaryFile)),
            (
                "a",
                "a",
                false,
                Side::New,
                500,
                Err(InlineBlock::LineNotInDiff),
            ),
            ("a", "a", false, Side::New, 11, Ok(())),
            ("a", "a", false, Side::Old, 11, Ok(())),
            ("a", "a", false, Side::New, 15, Ok(())),
            ("a", "a", false, Side::Old, 12, Ok(())),
            (
                "a",
                "a",
                false,
                Side::New,
                41,
                Err(InlineBlock::LineNotInDiff),
            ),
        ];
        for (sh, lh, bin, side, line, want) in table {
            assert_eq!(
                old_rule(sh, lh, &rows(), bin, side, line),
                want,
                "{sh} {lh} {line}"
            );
        }
    }

    #[test]
    fn head_commit_model_blocks_on_dest_branch_change() {
        let (s, l) = revs("h", None, "main");
        assert_eq!(check(AnchorModel::HeadCommit, &s, &l), Ok(()));
        let (s, l) = revs("h", None, "release");
        assert_eq!(
            check(AnchorModel::HeadCommit, &s, &l),
            Err(InlineBlock::BaseMismatch)
        );
        let (s, l) = revs("h", None, "");
        assert_eq!(
            check(AnchorModel::HeadCommit, &s, &l),
            Err(InlineBlock::BaseMismatch)
        );
        let (s, l) = revs("moved", None, "main");
        assert_eq!(
            check(AnchorModel::HeadCommit, &s, &l),
            Err(InlineBlock::HeadMismatch)
        );
    }

    #[test]
    fn diff_refs_model_blocks_on_base_mismatch() {
        let (s, l) = revs("h", Some("mb"), "whatever");
        assert_eq!(check(AnchorModel::DiffRefs, &s, &l), Ok(()));
        let (s, l) = revs("h", Some("other"), "main");
        assert_eq!(
            check(AnchorModel::DiffRefs, &s, &l),
            Err(InlineBlock::BaseMismatch)
        );
    }

    #[test]
    fn diff_refs_model_blocks_on_empty_server_shas() {
        for base in [None, Some("")] {
            let (s, l) = revs("h", base, "main");
            assert_eq!(
                check(AnchorModel::DiffRefs, &s, &l),
                Err(InlineBlock::BaseMismatch)
            );
        }
        let (s, l) = revs("", Some("mb"), "main");
        assert_eq!(
            check(AnchorModel::DiffRefs, &s, &l),
            Err(InlineBlock::HeadMismatch)
        );
    }

    #[test]
    fn multi_line_start_outside_rows_is_blocked() {
        let (s, l) = revs("h", None, "main");
        let rows = vec![
            row(RowKind::HunkHeader, None, None),
            row(RowKind::Context, Some(1), Some(1)),
            row(RowKind::Add, None, Some(2)),
            row(RowKind::Add, None, Some(3)),
            row(RowKind::HunkHeader, None, None),
            row(RowKind::Context, Some(40), Some(42)),
        ];
        let at = |start| {
            inline_allowed(
                AnchorModel::HeadCommit,
                &s,
                &l,
                "main",
                &rows,
                false,
                Side::New,
                3,
                start,
            )
        };
        assert_eq!(at(Some((Side::New, 1))), Ok(()));
        assert_eq!(at(Some((Side::Old, 1))), Ok(()));
        assert_eq!(at(Some((Side::New, 3))), Ok(()));
        assert_eq!(
            at(Some((Side::New, 9))),
            Err(InlineBlock::LineNotInDiff),
            "not in rows"
        );
        let across = inline_allowed(
            AnchorModel::HeadCommit,
            &s,
            &l,
            "main",
            &rows,
            false,
            Side::New,
            42,
            Some((Side::New, 2)),
        );
        assert_eq!(across, Err(InlineBlock::LineNotInDiff), "other hunk");
        let after = Some((Side::New, 42));
        assert_eq!(
            at(after),
            Err(InlineBlock::LineNotInDiff),
            "start after end"
        );
    }

    #[test]
    fn inline_blocked_on_head_mismatch() {
        assert_eq!(
            old_rule("a", "b", &rows(), false, Side::New, 11),
            Err(InlineBlock::HeadMismatch)
        );
        assert_eq!(
            old_rule("", "", &rows(), false, Side::New, 11),
            Err(InlineBlock::HeadMismatch)
        );
    }

    #[test]
    fn head_mismatch_wins_over_binary_and_binary_over_line() {
        assert_eq!(
            old_rule("a", "b", &rows(), true, Side::New, 999),
            Err(InlineBlock::HeadMismatch)
        );
        assert_eq!(
            old_rule("a", "a", &rows(), true, Side::New, 999),
            Err(InlineBlock::BinaryFile)
        );
    }

    #[test]
    fn non_code_rows_and_line_zero_never_validate() {
        for (side, line) in [
            (Side::New, 41),
            (Side::Old, 40),
            (Side::New, 43),
            (Side::Old, 42),
            (Side::New, 0),
            (Side::Old, 0),
        ] {
            assert_eq!(
                old_rule("a", "a", &rows(), false, side, line),
                Err(InlineBlock::LineNotInDiff)
            );
        }
    }

    #[test]
    fn inline_blocked_on_line_outside_diff() {
        assert_eq!(
            old_rule("a", "a", &rows(), false, Side::New, 500),
            Err(InlineBlock::LineNotInDiff)
        );
    }

    #[test]
    fn inline_allowed_for_added_line_new_side() {
        assert_eq!(old_rule("a", "a", &rows(), false, Side::New, 11), Ok(()));
    }

    #[test]
    fn inline_allowed_for_deleted_line_old_side() {
        assert_eq!(old_rule("a", "a", &rows(), false, Side::Old, 11), Ok(()));
        assert_eq!(
            old_rule("a", "a", &rows(), false, Side::Old, 13),
            Err(InlineBlock::LineNotInDiff)
        );
    }

    #[test]
    fn context_line_allowed_on_new_side_only_uses_new_number() {
        // Context row old 12 / new 15: each side uses its own number.
        assert_eq!(old_rule("a", "a", &rows(), false, Side::New, 15), Ok(()));
        assert_eq!(
            old_rule("a", "a", &rows(), false, Side::New, 12),
            Err(InlineBlock::LineNotInDiff)
        );
        assert_eq!(old_rule("a", "a", &rows(), false, Side::Old, 12), Ok(()));
        assert_eq!(
            old_rule("a", "a", &rows(), false, Side::Old, 15),
            Err(InlineBlock::LineNotInDiff)
        );
    }

    fn pr(id: u64, state: PrState, reviewers: Vec<ReviewState>) -> PullRequest {
        let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let repo = RepoRef::parse_bitbucket("acme/api").unwrap();
        PullRequest {
            repo: repo.clone(),
            id,
            title: format!("pr {id}"),
            state,
            draft: false,
            author: user("a"),
            source_branch: format!("b{id}"),
            dest_branch: "main".into(),
            source_commit: "a".into(),
            dest_commit: "b".into(),
            source_repo: repo,
            comment_count: 0,
            task_count: 0,
            reviewers: reviewers
                .into_iter()
                .enumerate()
                .map(|(i, state)| Reviewer {
                    user: user(&format!("r{i}")),
                    state,
                })
                .collect(),
            created_on: t,
            updated_on: t,
            forge: Default::default(),
            account: Default::default(),
            web_url: String::new(),
            node_id: None,
            review_requested_from_me: false,
            locked: false,
        }
    }

    fn stack() -> Stack {
        Stack {
            repo: RepoRef::parse_bitbucket("acme/api").unwrap(),
            members: vec![1, 2],
            edges: vec![(1, 2)],
        }
    }

    fn map(prs: Vec<PullRequest>) -> HashMap<u64, PullRequest> {
        prs.into_iter().map(|p| (p.id, p)).collect()
    }

    #[test]
    fn approval_warning_when_parent_unapproved() {
        let prs = map(vec![
            pr(1, PrState::Open, vec![ReviewState::Pending]),
            pr(2, PrState::Open, vec![]),
        ]);
        let w = approval_warnings(&stack(), &prs, 2);
        assert_eq!(w.len(), 1);
        assert_eq!(
            w,
            vec!["Parent PR #1 is not approved or merged yet; it should land first."]
        );
    }

    #[test]
    fn no_warning_when_parent_approved_or_merged_or_root() {
        let approved = map(vec![
            pr(1, PrState::Open, vec![ReviewState::Approved]),
            pr(2, PrState::Open, vec![]),
        ]);
        assert!(approval_warnings(&stack(), &approved, 2).is_empty());
        let merged = map(vec![
            pr(1, PrState::Merged, vec![]),
            pr(2, PrState::Open, vec![]),
        ]);
        assert!(approval_warnings(&stack(), &merged, 2).is_empty());
        assert!(approval_warnings(&stack(), &approved, 1).is_empty());
        assert!(approval_warnings(&stack(), &approved, 42).is_empty());
        // An absent parent PR (not in the map) yields no warning.
        let missing = map(vec![pr(2, PrState::Open, vec![])]);
        assert!(approval_warnings(&stack(), &missing, 2).is_empty());
    }
}
