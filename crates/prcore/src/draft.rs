use serde::{Deserialize, Serialize};

use crate::comment_type::{with_prefix, CommentType};
use crate::comments::{Thread, ThreadId};
use crate::model::RepoRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DraftKind {
    Pr,
    File,
    Inline,
    Reply,
}

/// A locally stored, not yet posted comment. Holds only comment text and anchors, never secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    /// Row id; `0` (or any value <= 0) means "not yet saved".
    pub id: i64,
    pub repo: RepoRef,
    pub pr_id: u64,
    pub kind: DraftKind,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub side: Option<Side>,
    /// First line of a multi-line inline draft and its side (`line` is the last); None for
    /// one line.
    #[serde(default)]
    pub start: Option<(Side, u32)>,
    /// For a reply: the thread it answers (`Thread::id`).
    pub reply_to: Option<ThreadId>,
    pub text: String,
    pub source_commit: String,
    /// Optional type, posted as a leading `**type:** ` prefix (see `Draft::posted_text`).
    #[serde(default)]
    pub comment_type: Option<CommentType>,
}

impl Draft {
    /// The exact text sent to the server: `text` behind the type's prefix (never doubled).
    pub fn posted_text(&self) -> String {
        with_prefix(self.comment_type, &self.text)
    }

    /// Whether this is a reply draft written on thread `t`.
    pub fn replies_to(&self, t: &Thread) -> bool {
        self.kind == DraftKind::Reply && self.reply_to.is_some() && self.reply_to.as_ref() == t.id()
    }
}
