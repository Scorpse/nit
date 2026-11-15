//! Markdown export of a review: the PR, then numbered comment items anchored `path:line`,
//! for pasting into notes or an LLM. Pure and deterministic: the same input gives the same
//! text, byte for byte.

use std::collections::{HashMap, HashSet};

use crate::comment_type::{split_prefix, CommentType};
use crate::comments::{Comment, CommentId, Thread, ThreadId};
use crate::commit::same_commit;
use crate::diff::{Row, RowKind};
use crate::draft::{Draft, DraftKind, Side};
use crate::model::{PullRequest, UserRef};

/// Which comments an export holds. Unsent drafts are in every scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExportScope {
    /// Only my unsent drafts.
    MyDrafts,
    /// Unresolved threads and my drafts.
    #[default]
    Unresolved,
    /// Every thread, resolved ones included, and my drafts.
    Everything,
}

/// Every scope with its menu label, in menu order.
pub const EXPORT_SCOPES: [(ExportScope, &str); 3] = [
    (ExportScope::MyDrafts, "My drafts only"),
    (ExportScope::Unresolved, "All comments (unresolved)"),
    (ExportScope::Everything, "Everything"),
];

impl ExportScope {
    pub fn label(self) -> &'static str {
        EXPORT_SCOPES
            .iter()
            .find(|(s, _)| *s == self)
            .map_or("", |(_, l)| l)
    }
}

/// What an export reads. Nothing here is fetched: what is not loaded is left out.
pub struct ExportInput<'a> {
    pub pr: &'a PullRequest,
    pub threads: &'a [Thread],
    pub drafts: &'a [Draft],
    /// The signed-in user, named as the author of the drafts ("You" when unknown).
    pub me: Option<&'a UserRef>,
    /// Paths of the current diff (renamed files' old paths included); None while unknown,
    /// then no comment is marked "file not in diff".
    pub diff_paths: Option<&'a HashSet<String>>,
    /// Diff rows of the files whose patch is loaded; the code excerpts come from these.
    pub patches: &'a HashMap<String, &'a [Row]>,
}

/// Rows of code shown before the anchored row in an excerpt.
pub const EXCERPT_CONTEXT: usize = 2;

/// Where an item sits.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Loc {
    Pr,
    File(String),
    Line {
        path: String,
        side: Side,
        line: u32,
    },
    /// A reply draft whose thread is unknown.
    Reply(ThreadId),
}

impl Loc {
    fn of_comment(c: &Comment) -> Loc {
        match &c.anchor {
            None => Loc::Pr,
            Some(a) => match (a.to, a.from) {
                (Some(n), _) => Loc::Line {
                    path: a.path.clone(),
                    side: Side::New,
                    line: n,
                },
                (None, Some(n)) => Loc::Line {
                    path: a.path.clone(),
                    side: Side::Old,
                    line: n,
                },
                (None, None) => Loc::File(a.path.clone()),
            },
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            Loc::File(p) | Loc::Line { path: p, .. } => Some(p),
            Loc::Pr | Loc::Reply(_) => None,
        }
    }

    /// `path:12`, `path:old 12`, `path (file)`, `PR-level`.
    fn label(&self) -> String {
        match self {
            Loc::Pr => "PR-level".into(),
            Loc::File(p) => format!("{} (file)", code(p)),
            Loc::Line {
                path,
                side: Side::New,
                line,
            } => code(&format!("{path}:{line}")),
            Loc::Line {
                path,
                side: Side::Old,
                line,
            } => code(&format!("{path}:old {line}")),
            Loc::Reply(id) => format!("Reply to comment #{id}"),
        }
    }

    /// PR-level first, then files by path (file-level before lines, lines in order, the new
    /// side before the old one), unknown replies last.
    fn sort_key(&self) -> (u8, &str, u8, u32, u8) {
        match self {
            Loc::Pr => (0, "", 0, 0, 0),
            Loc::File(p) => (1, p, 0, 0, 0),
            Loc::Line { path, side, line } => (1, path, 1, *line, (*side == Side::Old) as u8),
            Loc::Reply(id) => match id.as_u64() {
                Some(n) => (2, "", 0, n.min(u32::MAX as u64) as u32, 0),
                None => (2, id.as_str(), 0, u32::MAX, 0),
            },
        }
    }
}

enum Body<'a> {
    /// A thread with the reply drafts written on it.
    Thread(&'a Thread, Vec<&'a Draft>),
    /// A draft; for a reply, the thread it answers when that thread is known.
    Draft(&'a Draft, Option<&'a Thread>),
}

struct Item<'a> {
    loc: Loc,
    body: Body<'a>,
}

impl Item<'_> {
    fn order(&self) -> (u8, i64, CommentId) {
        match &self.body {
            Body::Thread(t, _) => (0, t.root.created_on.timestamp(), t.root.id.clone()),
            Body::Draft(d, _) => (1, 0, CommentId::from(d.id.max(0) as u64)),
        }
    }
}

/// A thread counts as gone when its root and every reply are deleted.
fn gone(t: &Thread) -> bool {
    t.root.deleted && t.replies.iter().all(|r| r.deleted)
}

fn draft_loc(d: &Draft, threads: &[Thread]) -> Loc {
    match d.kind {
        DraftKind::Pr => Loc::Pr,
        DraftKind::File => d.path.clone().map_or(Loc::Pr, Loc::File),
        DraftKind::Inline => match (&d.path, d.side, d.line) {
            (Some(p), Some(side), Some(line)) => Loc::Line {
                path: p.clone(),
                side,
                line,
            },
            (Some(p), ..) => Loc::File(p.clone()),
            _ => Loc::Pr,
        },
        DraftKind::Reply => {
            let parent = d.reply_to.clone().unwrap_or_default();
            threads
                .iter()
                .find(|t| d.replies_to(t))
                .map_or(Loc::Reply(parent), |t| Loc::of_comment(&t.root))
        }
    }
}

/// The review as markdown, for `scope`.
pub fn export_markdown(input: &ExportInput, scope: ExportScope) -> String {
    let threads: Vec<&Thread> = match scope {
        ExportScope::MyDrafts => Vec::new(),
        ExportScope::Unresolved => input
            .threads
            .iter()
            .filter(|t| !gone(t) && !t.root.resolved)
            .collect(),
        ExportScope::Everything => input.threads.iter().filter(|t| !gone(t)).collect(),
    };
    let mut drafts: Vec<&Draft> = input.drafts.iter().collect();
    drafts.sort_by_key(|d| d.id);
    let mut items: Vec<Item> = threads
        .iter()
        .map(|t| {
            let replies = drafts.iter().filter(|d| d.replies_to(t)).copied().collect();
            Item {
                loc: Loc::of_comment(&t.root),
                body: Body::Thread(t, replies),
            }
        })
        .collect();
    for d in &drafts {
        let attached = threads.iter().any(|t| d.replies_to(t));
        if attached {
            continue;
        }
        let parent = (d.kind == DraftKind::Reply)
            .then(|| input.threads.iter().find(|t| d.replies_to(t)))
            .flatten();
        items.push(Item {
            loc: draft_loc(d, input.threads),
            body: Body::Draft(d, parent),
        });
    }
    items.sort_by(|a, b| {
        a.loc
            .sort_key()
            .cmp(&b.loc.sort_key())
            .then(a.order().cmp(&b.order()))
    });

    let mut out = header(input, scope, items.len());
    if items.is_empty() {
        out.push_str("_No comments in this scope._\n");
    }
    for (i, item) in items.iter().enumerate() {
        item_md(&mut out, input, i + 1, item);
    }
    // every block ends with a blank line; the text ends with one newline
    let end = out.trim_end().len();
    out.truncate(end);
    out.push('\n');
    out
}

fn header(input: &ExportInput, scope: ExportScope, n: usize) -> String {
    let pr = input.pr;
    let head: String = pr.source_commit.trim().chars().take(7).collect();
    let head = if head.is_empty() {
        "unknown".to_string()
    } else {
        code(&head)
    };
    format!(
        "# PR #{}: {}\n\n\
         - Repository: {}\n\
         - Branches: {} -> {}\n\
         - Head: {}\n\
         - Scope: {}\n\
         - Items: {}\n\n",
        pr.id,
        one_line(&pr.title),
        code(&pr.repo.full_name()),
        code(&pr.source_branch),
        code(&pr.dest_branch),
        head,
        scope.label(),
        n
    )
}

fn item_md(out: &mut String, input: &ExportInput, n: usize, item: &Item) {
    let path_gone = item
        .loc
        .path()
        .zip(input.diff_paths)
        .is_some_and(|(p, set)| !set.contains(p));
    let (lead_type, outdated) = match &item.body {
        Body::Thread(t, _) => (
            comment_type(&t.root),
            t.root.anchor.as_ref().is_some_and(|a| a.outdated),
        ),
        Body::Draft(d, _) => (
            d.comment_type,
            d.kind == DraftKind::Inline && !same_commit(&d.source_commit, &input.pr.source_commit),
        ),
    };
    out.push_str(&format!("## {n}. {}", item.loc.label()));
    if let Some(t) = lead_type {
        out.push_str(&format!(" [{}]", t.as_str()));
    }
    out.push_str("\n\n");

    let mut marks: Vec<&str> = Vec::new();
    let author = match &item.body {
        Body::Thread(t, _) => {
            marks.push(if t.root.resolved {
                "resolved"
            } else {
                "unresolved"
            });
            author_of(&t.root)
        }
        Body::Draft(..) => {
            marks.push("draft");
            my_name(input)
        }
    };
    if outdated {
        marks.push("outdated");
    }
    if path_gone {
        marks.push("file not in diff");
    }
    out.push_str(&format!(
        "**{}** · {}\n\n",
        one_line(&author),
        marks.join(" · ")
    ));
    // a reply shown on its own (its thread is out of scope); an unknown thread's id is in
    // the heading already
    if let Body::Draft(_, Some(t)) = &item.body {
        out.push_str(&format!(
            "Reply to the thread by **{}**\n\n",
            one_line(&author_of(&t.root))
        ));
    }
    let text = match &item.body {
        Body::Thread(t, _) => shown_text(&t.root),
        Body::Draft(d, _) => d.text.clone(),
    };
    quote(out, "", &text);

    // the anchored line, only when its patch is loaded and the line still means that code
    if !outdated && !path_gone {
        if let Loc::Line { path, side, line } = &item.loc {
            if let Some(rows) = input.patches.get(path) {
                excerpt(out, rows, *side, *line);
            }
        }
    }

    let mut replies: Vec<(String, Option<CommentType>, Vec<&str>, String)> = Vec::new();
    if let Body::Thread(t, drafts) = &item.body {
        for r in &t.replies {
            let marks = if r.deleted { vec!["deleted"] } else { vec![] };
            replies.push((author_of(r), comment_type(r), marks, shown_text(r)));
        }
        for d in drafts {
            replies.push((
                my_name(input),
                d.comment_type,
                vec!["draft"],
                d.text.clone(),
            ));
        }
    }
    if !replies.is_empty() {
        out.push_str("Replies:\n\n");
        for (who, t, marks, text) in replies {
            out.push_str(&format!("- **{}**", one_line(&who)));
            if let Some(t) = t {
                out.push_str(&format!(" [{}]", t.as_str()));
            }
            for m in marks {
                out.push_str(&format!(" · {m}"));
            }
            out.push('\n');
            quote(out, "  ", &text);
        }
    }
}

fn comment_type(c: &Comment) -> Option<CommentType> {
    if c.deleted {
        None
    } else {
        split_prefix(&c.body).0
    }
}

/// The body without its type prefix (the type is shown as a tag), or `[deleted]`.
fn shown_text(c: &Comment) -> String {
    if c.deleted {
        "[deleted]".into()
    } else {
        split_prefix(&c.body).1.to_string()
    }
}

fn author_of(c: &Comment) -> String {
    if c.author_is_bot {
        format!("{} (bot)", c.author.display_name)
    } else {
        c.author.display_name.clone()
    }
}

fn my_name(input: &ExportInput) -> String {
    input
        .me
        .map(|m| m.display_name.trim())
        .filter(|n| !n.is_empty())
        .unwrap_or("You")
        .to_string()
}

/// `text` as a block quote, every line behind `indent` and `> `; a code fence inside the
/// comment ends with the quote, so it cannot swallow the rest of the export.
fn quote(out: &mut String, indent: &str, text: &str) {
    let text = text.replace("\r\n", "\n");
    let text = text.trim_end();
    let text = if text.trim().is_empty() {
        "(empty)"
    } else {
        text
    };
    for line in text.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            out.push_str(&format!("{indent}>\n"));
        } else {
            out.push_str(&format!("{indent}> {line}\n"));
        }
    }
    out.push('\n');
}

/// The anchored row and up to `EXCERPT_CONTEXT` code rows before it in its hunk, as a
/// fenced `diff` block whose fence is longer than any backtick run in the code.
fn excerpt(out: &mut String, rows: &[Row], side: Side, line: u32) {
    let hit = rows.iter().position(|r| match (side, r.kind) {
        (Side::New, RowKind::Context | RowKind::Add) => r.new_line == Some(line),
        (Side::Old, RowKind::Context | RowKind::Del) => r.old_line == Some(line),
        _ => false,
    });
    let Some(hit) = hit else { return };
    let mut start = hit;
    while start > 0
        && hit - start < EXCERPT_CONTEXT
        && matches!(
            rows[start - 1].kind,
            RowKind::Context | RowKind::Add | RowKind::Del
        )
    {
        start -= 1;
    }
    let lines: Vec<String> = rows[start..=hit]
        .iter()
        .map(|r| {
            let mark = match r.kind {
                RowKind::Add => '+',
                RowKind::Del => '-',
                _ => ' ',
            };
            format!("{mark}{}", r.text.trim_end_matches(['\r', '\n']))
        })
        .collect();
    let fence = "`".repeat(3.max(longest_run(&lines.concat(), '`') + 1));
    out.push_str(&format!("{fence}diff\n"));
    for l in &lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("{fence}\n\n"));
}

fn longest_run(s: &str, c: char) -> usize {
    let (mut best, mut cur) = (0, 0);
    for ch in s.chars() {
        if ch == c {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// `s` as an inline code span whose delimiter is longer than any backtick run inside.
fn code(s: &str) -> String {
    let s = one_line(s);
    let ticks = "`".repeat(longest_run(&s, '`') + 1);
    let pad = if s.starts_with('`') || s.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{s}{pad}{ticks}")
}

/// Newlines (and runs of whitespace around them) as single spaces: headings and list
/// lines stay one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests;
