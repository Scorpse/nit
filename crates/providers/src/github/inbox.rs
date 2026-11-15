//! Decoding GitHub pull requests (GraphQL list and REST shapes) and "needs me".
//! Entries are read field by field: one bad entry is skipped with its number and the path
//! of the offending field, never a value.

use chrono::{DateTime, Utc};
use prcore::comments::{looks_like_bot_name, UNKNOWN_AUTHOR};
use prcore::model::{
    AccountId, DiffStat, Forge, PrState, PullRequest, RepoRef, ReviewState, Reviewer, UserRef,
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::SkippedPr;

/// The teams the signed-in user belongs to: `(org, slug)`, lower-case.
pub type Teams = Vec<(String, String)>;

pub(crate) fn field<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |node, key| node.get(key))
}

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    field(v, path).and_then(Value::as_str)
}

pub(crate) fn time_at(v: &Value, path: &str) -> Option<DateTime<Utc>> {
    field(v, path).and_then(|t| DateTime::<Utc>::deserialize(t).ok())
}

fn u32_at(v: &Value, path: &str) -> Option<u32> {
    field(v, path)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// A user from a GraphQL actor (`login`, `databaseId`, `name`, `__typename`) or a REST user
/// (`login`, `id`, `type`). Missing actor (deleted user): unknown, empty id.
/// Bot: `Bot` type, a `[bot]` login, or a tool-like name (`looks_like_bot_name`).
pub(crate) fn actor(v: Option<&Value>) -> (UserRef, bool) {
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
    let login = str_at(u, "login").filter(|s| !s.trim().is_empty());
    let id = u
        .get("databaseId")
        .or_else(|| u.get("id"))
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_default();
    let name = str_at(u, "name")
        .filter(|s| !s.trim().is_empty())
        .or(login)
        .unwrap_or(UNKNOWN_AUTHOR);
    let kind = str_at(u, "__typename").or_else(|| str_at(u, "type"));
    let bot = kind == Some("Bot")
        || login.is_some_and(|l| l.ends_with("[bot]") || looks_like_bot_name(l));
    (
        UserRef {
            id,
            display_name: name.trim().into(),
            login: login.map(str::to_string),
        },
        bot,
    )
}

/// Same person: the numeric id when both have one, else the login (case-insensitive).
pub(crate) fn same_user(a: &UserRef, b: &UserRef) -> bool {
    if !a.id.is_empty() && !b.id.is_empty() {
        return a.id == b.id;
    }
    match (&a.login, &b.login) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

fn review_state(s: Option<&str>) -> ReviewState {
    match s {
        Some("APPROVED") => ReviewState::Approved,
        Some("CHANGES_REQUESTED") => ReviewState::ChangesRequested,
        Some("COMMENTED") => ReviewState::Commented,
        // DISMISSED, PENDING and anything new
        _ => ReviewState::Pending,
    }
}

/// `owner/name` on the account's host; a missing or odd value is the "unknown" repo (a
/// deleted fork must never look like the base repository).
fn repo_on(host: &str, name_with_owner: Option<&str>) -> RepoRef {
    name_with_owner
        .and_then(|n| RepoRef::parse(host, n))
        .filter(|r| !r.namespace.contains('/'))
        .unwrap_or_else(|| RepoRef {
            host: host.into(),
            namespace: "unknown".into(),
            slug: "unknown".into(),
        })
}

/// The first required field that is missing or of the wrong type.
/// A required field's path and its type check.
type Required = (&'static str, fn(&Value) -> bool);

fn missing(v: &Value, required: &[Required]) -> Option<String> {
    required
        .iter()
        .find(|(p, ok)| !field(v, p).is_some_and(ok))
        .map(|(p, _)| p.to_string())
}

fn is_str(v: &Value) -> bool {
    v.is_string()
}
fn is_u64(v: &Value) -> bool {
    v.as_u64().is_some()
}
fn is_time(v: &Value) -> bool {
    DateTime::<Utc>::deserialize(v).is_ok()
}

fn skipped(v: &Value, id_key: &str, field: String) -> SkippedPr {
    SkippedPr {
        id: v.get(id_key).and_then(Value::as_u64),
        field,
    }
}

fn base_pr(repo: &RepoRef) -> PullRequest {
    PullRequest {
        repo: repo.clone(),
        id: 0,
        title: String::new(),
        state: PrState::Open,
        draft: false,
        author: UserRef::default(),
        source_branch: String::new(),
        dest_branch: String::new(),
        source_commit: String::new(),
        dest_commit: String::new(),
        source_repo: repo.clone(),
        comment_count: 0,
        task_count: 0,
        reviewers: Vec::new(),
        created_on: DateTime::<Utc>::default(),
        updated_on: DateTime::<Utc>::default(),
        forge: Forge::GitHub,
        account: AccountId::default(),
        web_url: String::new(),
        node_id: None,
        review_requested_from_me: false,
        locked: false,
    }
}

/// One node of the `OPEN_PRS` query: the PR and its size (when the size fields are there).
pub(crate) fn pr_from_graphql(
    v: &Value,
    repo: &RepoRef,
    me: Option<&UserRef>,
    my_teams: &Teams,
) -> Result<(PullRequest, Option<DiffStat>), SkippedPr> {
    let required: [Required; 6] = [
        ("number", is_u64),
        ("title", is_str),
        ("headRefName", is_str),
        ("baseRefName", is_str),
        ("createdAt", is_time),
        ("updatedAt", is_time),
    ];
    if let Some(f) = missing(v, &required) {
        return Err(skipped(v, "number", f));
    }
    let mut pr = base_pr(repo);
    pr.id = v["number"].as_u64().unwrap_or_default();
    pr.title = str_at(v, "title").unwrap_or_default().into();
    pr.draft = v["isDraft"].as_bool().unwrap_or(false);
    pr.locked = v["locked"].as_bool().unwrap_or(false);
    pr.author = actor(v.get("author")).0;
    pr.source_branch = str_at(v, "headRefName").unwrap_or_default().into();
    pr.dest_branch = str_at(v, "baseRefName").unwrap_or_default().into();
    pr.source_commit = str_at(v, "headRefOid").unwrap_or_default().into();
    pr.dest_commit = str_at(v, "baseRefOid").unwrap_or_default().into();
    if v["isCrossRepository"].as_bool() == Some(true) {
        pr.source_repo = repo_on(&repo.host, str_at(v, "headRepository.nameWithOwner"));
    }
    pr.comment_count = u32_at(v, "comments.totalCount")
        .unwrap_or(0)
        .saturating_add(u32_at(v, "reviewThreads.totalCount").unwrap_or(0));
    pr.created_on = time_at(v, "createdAt").unwrap_or_default();
    pr.updated_on = time_at(v, "updatedAt").unwrap_or_default();
    pr.web_url = str_at(v, "url").unwrap_or_default().into();
    pr.node_id = str_at(v, "id").map(str::to_string);

    let nodes = |path: &str| -> Vec<Value> {
        field(v, path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    // latest review per reviewer, then the open requests (a re-request makes them pending)
    let reviews = nodes("latestReviews.nodes");
    for r in &reviews {
        let (user, _) = actor(r.get("author"));
        pr.reviewers.push(Reviewer {
            user,
            state: review_state(str_at(r, "state")),
        });
    }
    let mut requested_me = false;
    for req in nodes("reviewRequests.nodes") {
        let Some(who) = req.get("requestedReviewer").filter(|w| w.is_object()) else {
            continue;
        };
        if str_at(who, "__typename") == Some("Team") {
            let org = str_at(who, "organization.login").unwrap_or_default();
            let slug = str_at(who, "slug").unwrap_or_default();
            let key = (org.to_ascii_lowercase(), slug.to_ascii_lowercase());
            requested_me |= my_teams.contains(&key);
            pr.reviewers.push(Reviewer {
                user: UserRef {
                    id: format!("team:{}/{}", key.0, key.1),
                    display_name: str_at(who, "name").unwrap_or(slug).into(),
                    login: Some(slug.into()),
                },
                state: ReviewState::Pending,
            });
            continue;
        }
        let (user, _) = actor(Some(who));
        requested_me |= me.is_some_and(|m| same_user(m, &user));
        match pr.reviewers.iter_mut().find(|r| same_user(&r.user, &user)) {
            Some(r) => r.state = ReviewState::Pending,
            None => pr.reviewers.push(Reviewer {
                user,
                state: ReviewState::Pending,
            }),
        }
    }
    // A review of mine on an older head that did not approve: new commits want a look.
    let stale_mine = me.is_some_and(|m| {
        reviews.iter().any(|r| {
            same_user(m, &actor(r.get("author")).0)
                && str_at(r, "state") != Some("APPROVED")
                && str_at(r, "commit.oid").is_some_and(|c| c != pr.source_commit)
        })
    });
    let mine = me.is_some_and(|m| same_user(m, &pr.author));
    pr.review_requested_from_me = !mine && (requested_me || stale_mine);

    let stat = match (
        u32_at(v, "changedFiles"),
        u32_at(v, "additions"),
        u32_at(v, "deletions"),
    ) {
        (Some(files), Some(lines_added), Some(lines_removed)) => Some(DiffStat {
            files,
            lines_added,
            lines_removed,
            // per-file sizes are not in the list; known once the PR is opened locally
            max_file_lines: 0,
        }),
        _ => None,
    };
    Ok((pr, stat))
}

/// A REST pull request (list or detail).
pub(crate) fn pr_from_rest(v: &Value, repo: &RepoRef) -> Result<PullRequest, SkippedPr> {
    let required: [Required; 6] = [
        ("number", is_u64),
        ("title", is_str),
        ("head.ref", is_str),
        ("base.ref", is_str),
        ("created_at", is_time),
        ("updated_at", is_time),
    ];
    if let Some(f) = missing(v, &required) {
        return Err(skipped(v, "number", f));
    }
    let mut pr = base_pr(repo);
    pr.id = v["number"].as_u64().unwrap_or_default();
    pr.title = str_at(v, "title").unwrap_or_default().into();
    pr.state = match (str_at(v, "state"), v.get("merged_at")) {
        (Some("open"), _) => PrState::Open,
        (_, Some(m)) if !m.is_null() => PrState::Merged,
        // closed without a merge
        _ => PrState::Declined,
    };
    pr.draft = v["draft"].as_bool().unwrap_or(false);
    pr.locked = v["locked"].as_bool().unwrap_or(false);
    pr.author = actor(v.get("user")).0;
    pr.source_branch = str_at(v, "head.ref").unwrap_or_default().into();
    pr.dest_branch = str_at(v, "base.ref").unwrap_or_default().into();
    pr.source_commit = str_at(v, "head.sha").unwrap_or_default().into();
    pr.dest_commit = str_at(v, "base.sha").unwrap_or_default().into();
    pr.source_repo = match field(v, "head.repo") {
        // no repository info at all: the base repository
        None => repo.clone(),
        // `head.repo: null`: the fork was deleted
        Some(Value::Null) => repo_on(&repo.host, None),
        Some(r) => match str_at(r, "full_name") {
            Some(h) if h.eq_ignore_ascii_case(&repo.full_name()) => repo.clone(),
            h => repo_on(&repo.host, h),
        },
    };
    pr.comment_count = u32_at(v, "comments")
        .unwrap_or(0)
        .saturating_add(u32_at(v, "review_comments").unwrap_or(0));
    pr.created_on = time_at(v, "created_at").unwrap_or_default();
    pr.updated_on = time_at(v, "updated_at").unwrap_or_default();
    pr.web_url = str_at(v, "html_url").unwrap_or_default().into();
    pr.node_id = str_at(v, "node_id").map(str::to_string);
    for u in v["requested_reviewers"].as_array().into_iter().flatten() {
        pr.reviewers.push(Reviewer {
            user: actor(Some(u)).0,
            state: ReviewState::Pending,
        });
    }
    Ok(pr)
}

/// Size from a REST pull request detail (`additions`, `deletions`, `changed_files`).
pub(crate) fn stat_from_rest(v: &Value) -> Option<DiffStat> {
    Some(DiffStat {
        files: u32_at(v, "changed_files")?,
        lines_added: u32_at(v, "additions")?,
        lines_removed: u32_at(v, "deletions")?,
        max_file_lines: 0,
    })
}
