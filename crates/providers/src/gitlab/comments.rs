//! GitLab discussions of one MR (spec 7.4): every note of a discussion shares its
//! `ThreadId` (the discussion id); comment ids are `note:<id>`; system notes are skipped.
//!
//! Diff notes are placed by their discussion's `position`: `new_line` only is an added line
//! (`to`), `old_line` only a removed one (`from`), both a context line (`to`); `line_range`
//! gives a multi-line start; `old_path` is kept when it differs (renames);
//! `position_type: file` is file-level. GitLab documents no outdated flag: a note is outdated
//! when its `head_sha` or `base_sha` differs from the MR's current ones (conservative).

use prcore::comments::{Anchor, Comment, CommentId, ThreadId};
use prcore::draft::Side;
use serde_json::Value;

use super::inbox::{str_at, time_at, user};
use crate::provider::{CommentList, ServerRevision, SkippedComment};

fn line(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .and_then(|n| u32::try_from(n).ok())
}

/// Where a diff note sits, judged against the MR's current revision.
fn anchor(pos: &Value, current: &ServerRevision) -> Option<Anchor> {
    let new_path = str_at(pos, "new_path").filter(|p| !p.is_empty());
    let old_path = str_at(pos, "old_path").filter(|p| !p.is_empty());
    let path = new_path.or(old_path)?.to_string();
    let head = str_at(pos, "head_sha").filter(|s| !s.is_empty());
    let base = str_at(pos, "base_sha").filter(|s| !s.is_empty());
    let outdated = head != Some(current.head.as_str())
        || current.base.as_deref().is_some_and(|b| base != Some(b));
    let mut a = Anchor {
        old_path: old_path.filter(|o| *o != path).map(str::to_string),
        path,
        commit: head.map(str::to_string),
        outdated,
        ..Anchor::default()
    };
    if str_at(pos, "position_type") == Some("file") {
        return Some(a);
    }
    let (old, new) = (line(pos, "old_line"), line(pos, "new_line"));
    match (old, new) {
        (_, Some(n)) => a.to = Some(n),
        (Some(o), None) => a.from = Some(o),
        (None, None) => {}
    }
    let end = new.map(|n| (Side::New, n)).or(old.map(|o| (Side::Old, o)));
    let range_line = |key: &str| {
        pos.get("line_range")
            .and_then(|r| r.get(key))
            .and_then(|s| {
                line(s, "new_line")
                    .map(|n| (Side::New, n))
                    .or(line(s, "old_line").map(|o| (Side::Old, o)))
            })
    };
    // GitLab moves a note's lines after a push but never its `line_range` (live 19.5.1): a
    // range whose end is not the note's own line is stale and not shown
    let range_end = range_line("end");
    let fresh = range_end.is_none() || range_end == end;
    a.start = range_line("start").filter(|s| fresh && Some(*s) != end);
    Some(a)
}

/// Every non-system note of `discussions`, in GitLab's order.
pub(crate) fn decode(discussions: &[Value], current: &ServerRevision) -> CommentList {
    let mut out = CommentList::default();
    for d in discussions {
        let notes: Vec<&Value> = d["notes"].as_array().into_iter().flatten().collect();
        let Some(thread) = str_at(d, "id").filter(|s| !s.is_empty()) else {
            for n in &notes {
                out.skipped.push(SkippedComment {
                    id: n["id"].as_u64(),
                    field: "discussion.id".into(),
                });
            }
            continue;
        };
        let thread = ThreadId(thread.to_string());
        // a discussion's notes share its first note's position
        let pos = notes
            .iter()
            .find_map(|n| n.get("position").filter(|p| p.is_object()));
        let anchor = pos.and_then(|p| anchor(p, current));
        let can_resolve = notes.first().and_then(|n| n["resolvable"].as_bool()) == Some(true);
        let resolved = notes.first().and_then(|n| n["resolved"].as_bool()) == Some(true);
        for n in notes {
            if n["system"].as_bool() == Some(true) {
                continue;
            }
            let Some(id) = n["id"].as_u64() else {
                out.skipped.push(SkippedComment {
                    id: None,
                    field: "id".into(),
                });
                continue;
            };
            let (author, author_is_bot) = user(n.get("author"));
            let created = time_at(n, "created_at");
            let updated = time_at(n, "updated_at");
            let created_on = created.or(updated).unwrap_or_default();
            out.comments.push(Comment {
                id: CommentId(format!("note:{id}")),
                thread: Some(thread.clone()),
                parent_id: None,
                can_resolve,
                author,
                body: str_at(n, "body").unwrap_or_default().to_string(),
                created_on,
                updated_on: updated.unwrap_or(created_on),
                deleted: false,
                resolved,
                anchor: anchor.clone(),
                author_is_bot,
            });
        }
    }
    out
}
