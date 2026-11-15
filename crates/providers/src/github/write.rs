//! GitHub writes (spec 7.3, plan Task 19): comments, replies, resolve and reviews.
//!
//! - PR comment: `POST /issues/{n}/comments {body}`;
//! - file comment: `POST /pulls/{n}/comments {body, commit_id, path, subject_type: "file"}`;
//! - inline: `POST /pulls/{n}/comments {body, commit_id, path, line, side, start_line?,
//!   start_side?}`, never `position` ([review comments]);
//! - reply: `POST /pulls/{n}/comments/{root}/replies {body}`, `root` from the `ThreadId`;
//! - resolve: GraphQL `resolveReviewThread` / `unresolveReviewThread` with the thread node id;
//! - review: `POST /pulls/{n}/reviews {event, body?, commit_id}` ([reviews]).
//!
//! `commit_id` is always the head the user reviewed (`PostRevision::head`), never "the
//! latest". Every request goes through `GhClient::write` (spaced, no redirects, no retry).
//!
//! [review comments]: https://docs.github.com/en/rest/pulls/comments?apiVersion=2022-11-28
//! [reviews]: https://docs.github.com/en/rest/pulls/reviews?apiVersion=2022-11-28

use prcore::comments::{Anchor, Comment, CommentId, NewComment, ReviewAction, ThreadId};
use prcore::draft::Side;
use serde_json::{json, Value};

use super::inbox::{actor, time_at};
use crate::provider::{PostRevision, ProviderError};

/// Whether the app may post inline (line-anchored) GitHub comments. Enabled by the owner on
/// 2026-11-14 after the live anchoring run: `line` +
/// `side` + the PR head as `commit_id` landed on the intended line in every case, and a line
/// outside the diff is refused (422), never placed elsewhere. Set to false to hold inline
/// drafts again (they stay saved and are offered as file or PR comments).
pub const INLINE_ENABLED_GITHUB: bool = true;

/// `"<node id>|<root comment id>"`, or the node id alone when the root id is unknown.
pub(crate) fn thread_id(node: &str, root: Option<u64>) -> ThreadId {
    match root {
        Some(r) => ThreadId(format!("{node}|{r}")),
        None => ThreadId(node.to_string()),
    }
}

/// The thread's GraphQL node id and its root review comment's REST id (when present).
fn split_thread(t: &ThreadId) -> (&str, Option<u64>) {
    match t.as_str().split_once('|') {
        Some((node, root)) => (node, root.parse().ok()),
        None => (t.as_str(), None),
    }
}

fn side_word(s: Side) -> &'static str {
    match s {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    }
}

fn invalid(why: &str) -> ProviderError {
    ProviderError::Invalid(why.into())
}

/// The request for `c` on PR `id`: path (relative to the API base) and body. Refuses, before
/// any request, what GitHub cannot take.
pub(crate) fn comment_request(
    repo_path: &str,
    id: u64,
    at: &PostRevision,
    c: &NewComment,
) -> Result<(String, Value), ProviderError> {
    let review_comments = format!("{repo_path}/pulls/{id}/comments");
    let head = || {
        let h = at.head.trim();
        if h.is_empty() {
            Err(invalid("the reviewed commit is not known"))
        } else {
            Ok(h.to_string())
        }
    };
    match c {
        NewComment::Pr { text } => Ok((
            format!("{repo_path}/issues/{id}/comments"),
            json!({ "body": text }),
        )),
        NewComment::File { path, text } => {
            if path.is_empty() {
                return Err(invalid("a file comment needs a path"));
            }
            Ok((
                review_comments,
                json!({ "body": text, "commit_id": head()?, "path": path, "subject_type": "file" }),
            ))
        }
        NewComment::Inline {
            path,
            side,
            line,
            start,
            text,
        } => {
            if path.is_empty() {
                return Err(invalid("an inline comment needs a path"));
            }
            if *line == 0 || start.is_some_and(|(_, l)| l == 0) {
                return Err(invalid("inline line numbers start at 1"));
            }
            let mut body = json!({
                "body": text,
                "commit_id": head()?,
                "path": path,
                "line": line,
                "side": side_word(*side),
            });
            if let Some((s_side, s_line)) = start.filter(|s| *s != (*side, *line)) {
                body["start_line"] = json!(s_line);
                body["start_side"] = json!(side_word(s_side));
            }
            Ok((review_comments, body))
        }
        NewComment::Reply { thread, text } => {
            let (_, root) = split_thread(thread);
            let root = root.ok_or_else(|| invalid("the thread's first comment is not known"))?;
            Ok((
                format!("{review_comments}/{root}/replies"),
                json!({ "body": text }),
            ))
        }
    }
}

/// The comment GitHub echoed for `c`. A reply joins `c`'s thread; a new review comment's
/// thread id is not in the REST answer, so it has none until the comments are read again.
pub(crate) fn posted_comment(v: &Value, c: &NewComment) -> Option<Comment> {
    match c {
        NewComment::Pr { .. } => super::comments::issue_comment(v),
        NewComment::Reply { thread, .. } => review_comment(v, Some(thread.clone())),
        NewComment::File { .. } | NewComment::Inline { .. } => review_comment(v, None),
    }
}

fn line(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .and_then(|n| u32::try_from(n).ok())
}

fn side(v: &Value, key: &str) -> Option<Side> {
    match v.get(key).and_then(Value::as_str) {
        Some("LEFT") => Some(Side::Old),
        Some("RIGHT") => Some(Side::New),
        _ => None,
    }
}

/// A REST review comment (`POST /pulls/{n}/comments` or `.../replies` answer).
fn review_comment(v: &Value, thread: Option<ThreadId>) -> Option<Comment> {
    let id = v["id"].as_u64()?;
    let (author, author_is_bot) = actor(v.get("user"));
    let created = time_at(v, "created_at");
    let updated = time_at(v, "updated_at");
    let created_on = created.or(updated).unwrap_or_default();
    let anchor = v["path"].as_str().map(|path| {
        let mut a = Anchor {
            path: path.to_string(),
            commit: v["commit_id"].as_str().map(str::to_string),
            ..Anchor::default()
        };
        if v["subject_type"].as_str() != Some("file") {
            let end_side = side(v, "side").unwrap_or(Side::New);
            let end = line(v, "line");
            match end_side {
                Side::Old => a.from = end,
                Side::New => a.to = end,
            }
            let start_side = side(v, "start_side").unwrap_or(end_side);
            a.start = line(v, "start_line")
                .filter(|&s| end.is_some_and(|e| s != e || start_side != end_side))
                .map(|s| (start_side, s));
        }
        a
    });
    Some(Comment {
        id: CommentId(format!("rc:{id}")),
        thread,
        parent_id: None,
        can_resolve: false,
        author,
        body: v["body"].as_str().unwrap_or_default().to_string(),
        created_on,
        updated_on: updated.unwrap_or(created_on),
        deleted: false,
        resolved: false,
        anchor,
        author_is_bot,
    })
}

/// The thread node id `set_resolved` sends.
pub(crate) fn resolve_target(t: &ThreadId) -> Result<&str, ProviderError> {
    let (node, _) = split_thread(t);
    if node.trim().is_empty() {
        return Err(invalid("the thread is not known"));
    }
    Ok(node)
}

/// The review `event` and its text, refusing before any request what GitHub does not take:
/// clearing an approval or a change request (there is no such review), and a change request
/// without text (GitHub requires one).
pub(crate) fn review_event(
    a: ReviewAction,
    body: Option<&str>,
) -> Result<(&'static str, Option<String>), ProviderError> {
    let text = body.filter(|b| !b.trim().is_empty()).map(str::to_string);
    match a {
        ReviewAction::Approve => Ok(("APPROVE", text)),
        ReviewAction::RequestChanges => match text {
            Some(t) => Ok(("REQUEST_CHANGES", Some(t))),
            None => Err(invalid("GitHub needs a comment to request changes")),
        },
        ReviewAction::ClearApproval | ReviewAction::ClearRequestChanges => {
            Err(invalid("this action is not available on this forge"))
        }
    }
}

/// `POST /pulls/{n}/reviews` body.
pub(crate) fn review_body(event: &str, text: Option<&str>, head: &str) -> Value {
    let mut body = json!({ "event": event, "commit_id": head });
    if let Some(t) = text {
        body["body"] = json!(t);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_round_trips() {
        let t = thread_id("PRRT_a", Some(101));
        assert_eq!(t.as_str(), "PRRT_a|101");
        assert_eq!(split_thread(&t), ("PRRT_a", Some(101)));
        assert_eq!(split_thread(&thread_id("PRRT_a", None)), ("PRRT_a", None));
        assert_eq!(resolve_target(&t).unwrap(), "PRRT_a");
        assert!(resolve_target(&ThreadId("|101".into())).is_err());
    }

    #[test]
    fn single_line_start_equal_to_the_line_is_not_sent() {
        let at = PostRevision {
            head: "abc".into(),
            merge_base: String::new(),
        };
        let c = NewComment::Inline {
            path: "a".into(),
            side: Side::New,
            line: 4,
            start: Some((Side::New, 4)),
            text: "t".into(),
        };
        let (_, body) = comment_request("/repos/o/r", 1, &at, &c).unwrap();
        assert!(body.get("start_line").is_none() && body.get("position").is_none());
        // no reviewed head: refused
        let none = PostRevision::default();
        assert!(comment_request("/repos/o/r", 1, &none, &c).is_err());
    }
}
