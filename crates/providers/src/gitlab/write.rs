//! GitLab writes (spec 7.3, 7.6, plan Task 27): notes, discussions, replies, resolve,
//! approvals and request changes.
//!
//! - MR comment: `POST .../notes {body}`;
//! - file comment: `POST .../discussions {body, position: {position_type: file, ...}}`;
//! - inline: `POST .../discussions {body, position}` with the SHAs of the diff version whose
//!   head is the reviewed commit and the line looked up in GitLab's diff of that version
//!   (`gitlab::position`);
//! - reply: `POST .../discussions/:id/notes {body}`;
//! - resolve: `PUT .../discussions/:id?resolved=true|false`;
//! - approve: `POST .../approve {sha}` (409 when the head moved); clear: `POST .../unapprove`;
//! - request changes: GraphQL `mergeRequestRequestChanges` (it publishes no pending draft
//!   notes, unlike `draft_notes/bulk_publish`). There is no reviewer-side "clear".
//!
//! Text with a line starting with `/` and a letter is refused before any request (decision
//! D7, also checked in the composer): GitLab would run it as a quick action.
//!
//! [discussions]: https://docs.gitlab.com/api/discussions/

use prcore::comments::{Comment, NewComment, ReviewAction, ServerRevision, ThreadId};
use prcore::draft::Side;
use serde_json::{json, Value};

use super::position::{self, DiffRefs};
use crate::provider::{PostRevision, ProviderError};

/// Whether the app may post inline (line-anchored) GitLab comments. Enabled on 2026-11-15
/// after the live anchoring run against GitLab 19.5.1: every added, context, deleted and
/// multi-line position landed on the intended line (GitLab accepts a text position only for
/// the exact old/new pair of a diff line, and suggestions replaced exactly the intended
/// lines), and a line outside the diff is refused (by nit before any request, and by GitLab
/// with a 400). Set to false to hold inline drafts again (they stay saved).
pub const INLINE_ENABLED_GITLAB: bool = true;

/// Why a draft that would run a quick action is refused.
pub const QUICK_ACTION: &str =
    "a line starting with \"/\" and a letter would run a GitLab quick action";

/// `RequestChanges` (GraphQL; `projectPath`, `iid`).
pub const REQUEST_CHANGES: &str = "mutation($projectPath: ID!, $iid: String!) { mergeRequestRequestChanges(input: {projectPath: $projectPath, iid: $iid}) { errors } }";

fn invalid(why: &str) -> ProviderError {
    ProviderError::Invalid(why.into())
}

/// Decision D7: refuse text with a line that starts (after blanks) with `/` and a letter.
pub(crate) fn quick_action_guard(text: &str) -> Result<(), ProviderError> {
    let runs = text.lines().any(|l| {
        let mut ch = l.trim_start_matches([' ', '\t']).chars();
        ch.next() == Some('/') && ch.next().is_some_and(|c| c.is_ascii_alphabetic())
    });
    if runs {
        Err(invalid(QUICK_ACTION))
    } else {
        Ok(())
    }
}

/// What a write needs from the MR's diff versions: the version whose head is the reviewed
/// commit. `versions` is the `/versions` list.
pub(crate) fn version_for<'a>(versions: &'a Value, head: &str) -> Result<&'a Value, ProviderError> {
    let head = head.trim();
    if head.is_empty() {
        return Err(invalid("the reviewed commit is not known"));
    }
    versions
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["head_commit_sha"].as_str() == Some(head))
        .max_by_key(|v| v["id"].as_u64().unwrap_or(0))
        .ok_or_else(|| {
            invalid("GitLab has no diff of the reviewed commit yet; reload the merge request")
        })
}

/// The SHAs of a `/versions` entry.
pub(crate) fn version_refs(v: &Value) -> Result<DiffRefs, ProviderError> {
    let s = |k: &str| {
        v[k].as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| invalid("GitLab is still preparing this merge request"))
    };
    Ok(DiffRefs {
        base: s("base_commit_sha")?,
        start: s("start_commit_sha")?,
        head: s("head_commit_sha")?,
    })
}

/// `(old_path, new_path, diff)` of `path` in a version's `diffs` (new path first).
pub(crate) fn file_in<'a>(version: &'a Value, path: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let list = version["diffs"].as_array()?;
    let f = list
        .iter()
        .find(|d| d["new_path"] == path)
        .or_else(|| list.iter().find(|d| d["old_path"] == path))?;
    Some((
        f["old_path"].as_str()?,
        f["new_path"].as_str()?,
        f["diff"].as_str().unwrap_or_default(),
    ))
}

/// The `position` of a file or inline comment, from the version of the reviewed head
/// (`version`: `/versions/:id` with `diffs`). Refuses, before any write, a file that is not
/// in that diff, a line in no hunk, and an inline comment whose reviewed merge base is not
/// the version's base.
pub(crate) fn position_for(
    version: &Value,
    at: &PostRevision,
    c: &NewComment,
) -> Result<Value, ProviderError> {
    let refs = version_refs(version)?;
    let (path, line) = match c {
        NewComment::File { path, .. } => (path, None),
        NewComment::Inline {
            path,
            side,
            line,
            start,
            ..
        } => (path, Some((*side, *line, *start))),
        _ => return Err(invalid("not a file or line comment")),
    };
    if path.is_empty() {
        return Err(invalid("a file or line comment needs a path"));
    }
    let (old_path, new_path, diff) = file_in(version, path)
        .ok_or_else(|| invalid("the file is not in the merge request's diff"))?;
    let Some((side, line, start)) = line else {
        return Ok(position::file_position(&refs, old_path, new_path));
    };
    let base = at.merge_base.trim();
    if !base.is_empty() && base != refs.base {
        return Err(invalid(
            "the merge request's base changed since it was reviewed; reload it",
        ));
    }
    if line == 0 || start.is_some_and(|(_, l)| l == 0) {
        return Err(invalid("inline line numbers start at 1"));
    }
    let find = |s: Side, n: u32| {
        position::locate(diff, s, n)
            .ok_or_else(|| invalid("the line is not in GitLab's diff of this merge request"))
    };
    let end = find(side, line)?;
    let start = start.map(|(s, n)| find(s, n)).transpose()?;
    Ok(position::text_position(
        &refs,
        old_path,
        new_path,
        &end,
        start.as_ref(),
    ))
}

/// `?resolved=` target: the discussion id, refused when empty or not a plain id.
pub(crate) fn discussion_id(t: &ThreadId) -> Result<&str, ProviderError> {
    let id = t.as_str();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(invalid("the discussion is not known"));
    }
    Ok(id)
}

/// The comment GitLab echoed: a discussion (file/inline; its first note, joined to it) or a
/// note (MR comment: no thread until the discussions are read again; reply: `thread`).
pub(crate) fn posted_comment(
    v: &Value,
    c: &NewComment,
    current: &ServerRevision,
) -> Option<Comment> {
    let discussion = match c {
        NewComment::File { .. } | NewComment::Inline { .. } => v.clone(),
        NewComment::Reply { thread, .. } => json!({ "id": thread.as_str(), "notes": [v] }),
        NewComment::Pr { .. } => json!({ "id": "pending", "notes": [v] }),
    };
    let mut list = super::comments::decode(std::slice::from_ref(&discussion), current);
    let mut first = list.comments.drain(..).next()?;
    if matches!(c, NewComment::Pr { .. }) {
        first.thread = None;
    }
    Some(first)
}

/// What a review action sends: `(path suffix under the MR, body)` for REST, None for the
/// GraphQL request-changes mutation. Refuses what GitLab has no reviewer action for.
pub(crate) fn review_request(
    a: ReviewAction,
    head: &str,
) -> Result<Option<(&'static str, Option<Value>)>, ProviderError> {
    match a {
        ReviewAction::Approve => {
            if head.trim().is_empty() {
                return Err(ProviderError::Decode("merge request without head".into()));
            }
            Ok(Some(("approve", Some(json!({ "sha": head })))))
        }
        ReviewAction::ClearApproval => Ok(Some(("unapprove", None))),
        ReviewAction::RequestChanges => Ok(None),
        ReviewAction::ClearRequestChanges => {
            Err(invalid("this action is not available on this forge"))
        }
    }
}

/// GitLab's answers to approve / unapprove, in words the user can act on.
pub(crate) fn review_error(a: ReviewAction, e: ProviderError) -> ProviderError {
    match (a, e) {
        (ReviewAction::Approve, ProviderError::Rejected { status: 409, .. }) => {
            ProviderError::Rejected {
                status: 409,
                message: "the merge request changed since it was loaded; reload it".into(),
            }
        }
        // the read of the MR just before succeeded, so this is not a bad token
        (ReviewAction::Approve, ProviderError::Auth) => ProviderError::Rejected {
            status: 401,
            message: "GitLab refused the approval (already approved, not allowed, or the project asks for your password: approve in the browser)".into(),
        },
        (
            ReviewAction::ClearApproval,
            ProviderError::NotFound | ProviderError::Rejected { status: 404, .. },
        ) => ProviderError::Rejected {
            status: 404,
            message: "there is no approval of yours to remove".into(),
        },
        (_, e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_lines_are_refused() {
        for bad in ["/approve", "ok\n/merge", "  /assign @me", "\t/label ~x"] {
            assert!(quick_action_guard(bad).is_err(), "{bad:?}");
        }
        for ok in ["a/b", "/ x", "//c", "/1", "path /etc", ""] {
            assert!(quick_action_guard(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn version_is_the_one_of_the_reviewed_head() {
        let versions = json!([
            {"id": 3, "head_commit_sha": "h2", "base_commit_sha": "b", "start_commit_sha": "s"},
            {"id": 1, "head_commit_sha": "h1", "base_commit_sha": "b", "start_commit_sha": "s"},
        ]);
        assert_eq!(version_for(&versions, "h1").unwrap()["id"], 1);
        assert!(version_for(&versions, "h9").is_err());
        assert!(version_for(&versions, " ").is_err());
        assert!(version_refs(
            &json!({"head_commit_sha": "h", "base_commit_sha": "", "start_commit_sha": "s"})
        )
        .is_err());
    }

    #[test]
    fn discussion_ids_are_plain() {
        assert_eq!(
            discussion_id(&ThreadId("0e189cd1944eef35".into())).unwrap(),
            "0e189cd1944eef35"
        );
        for bad in ["", "a/b", "x?resolved=false", "a b"] {
            assert!(discussion_id(&ThreadId(bad.into())).is_err(), "{bad}");
        }
    }

    #[test]
    fn review_errors_are_reworded() {
        let r = review_error(
            ReviewAction::Approve,
            ProviderError::Rejected {
                status: 409,
                message: "SHA does not match HEAD of source branch: abc".into(),
            },
        );
        assert!(
            matches!(r, ProviderError::Rejected { status: 409, message } if message.contains("reload"))
        );
        assert!(matches!(
            review_error(ReviewAction::Approve, ProviderError::Auth),
            ProviderError::Rejected { status: 401, .. }
        ));
        assert_eq!(
            review_error(ReviewAction::RequestChanges, ProviderError::Auth),
            ProviderError::Auth
        );
    }
}
