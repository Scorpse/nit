//! A PR's commits and the point of my last review: which commits are new since then.

use crate::commit::same_commit;
use chrono::{DateTime, Utc};

/// One commit of a PR, as `git log` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// Full id.
    pub id: String,
    pub author: String,
    /// Committer date: when it landed, which a rebase moves.
    pub date: DateTime<Utc>,
    /// First line of the message.
    pub subject: String,
}

/// Where my last submitted review stands: its time and, when the forge says, the commit it
/// was made on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewMark {
    pub at: DateTime<Utc>,
    pub commit: Option<String>,
}

/// `git log` format for `parse_log`: id, author, committer date (ISO 8601), subject; fields
/// separated by U+001F, entries by NUL (`-z`).
pub const LOG_FORMAT: &str = "--format=%H%x1f%an%x1f%cI%x1f%s";

/// The entries of `git log -z <LOG_FORMAT>`, in the order given. An entry that does not parse
/// is skipped.
pub fn parse_log(raw: &[u8]) -> Vec<CommitInfo> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter_map(|entry| {
            let mut f = entry.trim_start_matches('\n').splitn(4, '\u{1f}');
            let (id, author, date, subject) = (f.next()?, f.next()?, f.next()?, f.next()?);
            Some(CommitInfo {
                id: id.to_string(),
                author: author.to_string(),
                date: DateTime::parse_from_rfc3339(date).ok()?.with_timezone(&Utc),
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// For each commit (oldest first), whether it came after my last review. With the reviewed
/// commit among them, those after it; otherwise (a forge that gives no commit, or one a force
/// push removed) those dated after the review.
pub fn new_since(commits: &[CommitInfo], mark: &ReviewMark) -> Vec<bool> {
    let reviewed = mark
        .commit
        .as_deref()
        .and_then(|c| commits.iter().position(|k| same_commit(&k.id, c)));
    commits
        .iter()
        .enumerate()
        .map(|(i, c)| match reviewed {
            Some(r) => i > r,
            None => c.date > mark.at,
        })
        .collect()
}

#[cfg(test)]
mod tests;
