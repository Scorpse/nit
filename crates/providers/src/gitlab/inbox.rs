//! Decoding GitLab merge requests, projects, users and reviewer states, and "needs me".
//! Entries are read field by field: one bad entry is skipped with its iid and the path of
//! the offending field, never a value.

use chrono::{DateTime, Utc};
use prcore::comments::{looks_like_bot_name, UNKNOWN_AUTHOR};
use prcore::model::{
    AccountId, DiffStat, Forge, PrState, PullRequest, RepoInfo, RepoRef, ReviewState, Reviewer,
    UserRef,
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::SkippedPr;

/// What a `"1000+"` changes count becomes: one over GitLab's documented default file
/// limit, so the "over GitLab's default limits" badge shows; the inbox shows it as "1000+".
pub const CAPPED_FILES: u32 = 1001;

pub(crate) fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn time_at(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)
        .and_then(|t| DateTime::<Utc>::deserialize(t).ok())
}

/// A GitLab user (`id`, `username`, `name`, `bot`). Missing (deleted user): unknown, empty
/// id. Bot: `bot: true` (on `/user` examples; UNVERIFIED elsewhere) or a tool-like name.
pub(crate) fn user(v: Option<&Value>) -> (UserRef, bool) {
    let Some(u) = v.filter(|u| u.is_object()) else {
        return (
            UserRef {
                id: String::new(),
                display_name: UNKNOWN_AUTHOR.into(),
                login: None,
            },
            false,
        );
    };
    let login = str_at(u, "username").filter(|s| !s.trim().is_empty());
    let id = u
        .get("id")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_default();
    let name = str_at(u, "name")
        .filter(|s| !s.trim().is_empty())
        .or(login)
        .unwrap_or(UNKNOWN_AUTHOR);
    let bot = u["bot"].as_bool() == Some(true) || login.is_some_and(looks_like_bot_name);
    (
        UserRef {
            id,
            display_name: name.trim().into(),
            login: login.map(str::to_string),
        },
        bot,
    )
}

/// Same person: the numeric id (GitLab ids are never empty for real users).
pub(crate) fn same_user(a: &UserRef, b: &UserRef) -> bool {
    !a.id.is_empty() && a.id == b.id
}

/// A reviewer state of `/reviewers` (spec 2): `approved`, `requested_changes`, `reviewed`
/// (a review without a verdict); `unreviewed`, `review_started`, `unapproved` and anything
/// new are still pending.
pub(crate) fn review_state(s: Option<&str>) -> ReviewState {
    match s {
        Some("approved") => ReviewState::Approved,
        Some("requested_changes") => ReviewState::ChangesRequested,
        Some("reviewed") => ReviewState::Commented,
        _ => ReviewState::Pending,
    }
}

/// A project from `/projects`: its `path_with_namespace` on `host`.
pub(crate) fn project(v: &Value, host: &str) -> Option<RepoInfo> {
    let repo = RepoRef::parse(host, str_at(v, "path_with_namespace")?)?;
    Some(RepoInfo {
        repo,
        name: str_at(v, "name").map(str::to_string),
        // `simple=true` may leave it out (UNVERIFIED which fields it keeps)
        private: str_at(v, "visibility").map(|s| s != "public"),
    })
}

type Required = (&'static str, fn(&Value) -> bool);

fn is_str(v: &Value) -> bool {
    v.is_string()
}
fn is_u64(v: &Value) -> bool {
    v.as_u64().is_some()
}
fn is_time(v: &Value) -> bool {
    DateTime::<Utc>::deserialize(v).is_ok()
}

/// A merge request (list or single). The size and review state come separately.
pub(crate) fn mr(v: &Value, repo: &RepoRef) -> Result<PullRequest, SkippedPr> {
    let required: [Required; 6] = [
        ("iid", is_u64),
        ("title", is_str),
        ("source_branch", is_str),
        ("target_branch", is_str),
        ("created_at", is_time),
        ("updated_at", is_time),
    ];
    if let Some((f, _)) = required.iter().find(|(k, ok)| !v.get(*k).is_some_and(ok)) {
        return Err(SkippedPr {
            id: v.get("iid").and_then(Value::as_u64),
            field: f.to_string(),
        });
    }
    let (state, locked) = match str_at(v, "state") {
        Some("opened") => (PrState::Open, false),
        Some("locked") => (PrState::Open, true),
        Some("merged") => (PrState::Merged, false),
        _ => (PrState::Declined, false),
    };
    let diff_ref = |k: &str| {
        v.get("diff_refs")
            .and_then(|d| str_at(d, k))
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    let source_repo = match (
        v["source_project_id"].as_u64(),
        v["target_project_id"].as_u64(),
    ) {
        // a fork: the source project's path is not in the answer
        (Some(s), Some(t)) if s != t => RepoRef {
            host: repo.host.clone(),
            namespace: "unknown".into(),
            slug: format!("project-{s}"),
        },
        _ => repo.clone(),
    };
    let mut reviewers = Vec::new();
    for r in v["reviewers"].as_array().into_iter().flatten() {
        reviewers.push(Reviewer {
            user: user(Some(r)).0,
            state: ReviewState::Pending,
        });
    }
    Ok(PullRequest {
        repo: repo.clone(),
        id: v["iid"].as_u64().unwrap_or_default(),
        title: str_at(v, "title").unwrap_or_default().into(),
        state,
        draft: v["draft"].as_bool() == Some(true) || v["work_in_progress"].as_bool() == Some(true),
        author: user(v.get("author")).0,
        source_branch: str_at(v, "source_branch").unwrap_or_default().into(),
        dest_branch: str_at(v, "target_branch").unwrap_or_default().into(),
        source_commit: str_at(v, "sha")
            .map(str::to_string)
            .or_else(|| diff_ref("head_sha"))
            .unwrap_or_default(),
        dest_commit: diff_ref("start_sha").unwrap_or_default(),
        source_repo,
        comment_count: v["user_notes_count"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        task_count: 0,
        reviewers,
        created_on: time_at(v, "created_at").unwrap_or_default(),
        updated_on: time_at(v, "updated_at").unwrap_or_default(),
        forge: Forge::GitLab,
        account: AccountId::default(),
        web_url: str_at(v, "web_url").unwrap_or_default().into(),
        node_id: v["project_id"]
            .as_u64()
            .or(v["target_project_id"].as_u64())
            .map(|n| n.to_string()),
        review_requested_from_me: false,
        locked,
    })
}

/// Applies `/reviewers` and `/approvals` to `pr` and decides "needs me" (spec 8.2): I am a
/// reviewer whose state is still pending (not approved, not requested changes, not
/// reviewed), I have not approved, and it is not my own MR. A user who approved without
/// being a reviewer is listed as approved.
pub(crate) fn apply_review_state(
    pr: &mut PullRequest,
    reviewers: Option<&[Value]>,
    approvals: Option<&Value>,
    me: Option<&UserRef>,
) {
    if let Some(list) = reviewers {
        pr.reviewers = list
            .iter()
            .map(|r| Reviewer {
                user: user(r.get("user")).0,
                state: review_state(str_at(r, "state")),
            })
            .collect();
    }
    let approved_by: Vec<UserRef> = approvals
        .and_then(|a| a["approved_by"].as_array())
        .into_iter()
        .flatten()
        .map(|a| user(a.get("user")).0)
        .filter(|u| !u.id.is_empty())
        .collect();
    for u in approved_by.iter() {
        match pr.reviewers.iter_mut().find(|r| same_user(&r.user, u)) {
            Some(r) => r.state = ReviewState::Approved,
            None => pr.reviewers.push(Reviewer {
                user: u.clone(),
                state: ReviewState::Approved,
            }),
        }
    }
    let Some(me) = me else {
        pr.review_requested_from_me = false;
        return;
    };
    let mine = same_user(me, &pr.author);
    let i_approved = approvals.and_then(|a| a["user_has_approved"].as_bool()) == Some(true);
    let asked = pr
        .reviewers
        .iter()
        .any(|r| same_user(&r.user, me) && r.state == ReviewState::Pending);
    pr.review_requested_from_me = !mine && !i_approved && asked;
}

/// `changes_count` of a single MR: a number as a string, `"1000+"` when capped.
/// `(files, capped)`; None while GitLab has not computed it yet.
pub fn parse_changes_count(v: &Value) -> Option<(u32, bool)> {
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if let Some(n) = s.strip_suffix('+') {
        return n.parse::<u32>().ok().map(|_| (CAPPED_FILES, true));
    }
    s.parse::<u32>().ok().map(|n| (n, false))
}

/// Files only (decision D6): lines and the largest file are unknown until the MR is
/// opened locally.
pub(crate) fn stat(v: &Value) -> Option<DiffStat> {
    let (files, _) = parse_changes_count(v.get("changes_count")?)?;
    Some(DiffStat {
        files,
        lines_added: 0,
        lines_removed: 0,
        max_file_lines: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changes_count_forms() {
        assert_eq!(parse_changes_count(&json!("12")), Some((12, false)));
        assert_eq!(
            parse_changes_count(&json!("1000+")),
            Some((CAPPED_FILES, true))
        );
        assert_eq!(parse_changes_count(&json!(3)), Some((3, false)));
        assert_eq!(parse_changes_count(&json!(null)), None);
        assert_eq!(parse_changes_count(&json!("")), None);
    }

    #[test]
    fn reviewer_state_words() {
        assert_eq!(review_state(Some("approved")), ReviewState::Approved);
        assert_eq!(
            review_state(Some("requested_changes")),
            ReviewState::ChangesRequested
        );
        assert_eq!(review_state(Some("reviewed")), ReviewState::Commented);
        for s in ["unreviewed", "review_started", "unapproved", "new-word"] {
            assert_eq!(review_state(Some(s)), ReviewState::Pending, "{s}");
        }
    }
}
