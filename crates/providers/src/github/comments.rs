//! GitHub comments of one PR (spec 7.4): review threads from GraphQL (thread node id,
//! resolved, outdated, sides and multi-line ranges), issue comments from REST (flat, no
//! thread) and the summary bodies of submitted reviews (read-only PR-level comments).
//!
//! Ids: review comment `rc:<databaseId>`, issue comment `ic:<id>`, review `rv:<databaseId>`;
//! `ThreadId` is `"<thread node id>|<root review comment databaseId>"` (`write::thread_id`):
//! resolve needs the node id, a reply the root comment's REST id.

use prcore::comments::{Anchor, Comment, CommentId};
use prcore::draft::Side;
use prcore::model::RepoRef;
use serde_json::{json, Value};

use super::client::{GhClient, MAX_PAGES};
use super::graphql;
use super::inbox::{actor, field, time_at};
use crate::provider::{CommentList, ProviderError, SkippedComment};

pub(crate) async fn comments(
    client: &GhClient,
    repo: &RepoRef,
    id: u64,
) -> Result<CommentList, ProviderError> {
    let issue_path = format!(
        "/repos/{}/{}/issues/{id}/comments?per_page=100",
        super::enc(&repo.namespace),
        super::enc(&repo.slug)
    );
    let (threads, issue, reviews) = tokio::join!(
        review_threads(client, repo, id),
        client.get_paged(&issue_path),
        reviews(client, repo, id),
    );
    let mut out = CommentList::default();
    for (t, comments) in threads? {
        thread_comments(&t, &comments, &mut out);
    }
    for v in issue? {
        match issue_comment(&v) {
            Some(c) => out.comments.push(c),
            None => out.skipped.push(SkippedComment {
                id: None,
                field: "id".into(),
            }),
        }
    }
    // Review bodies are extra: when they cannot be read, the threads still show.
    for v in reviews.unwrap_or_default() {
        if let Some(c) = review_body(&v) {
            out.comments.push(c);
        }
    }
    Ok(out)
}

fn vars(repo: &RepoRef, id: u64, cursor: &Option<String>) -> Value {
    json!({"owner": repo.namespace, "name": repo.slug, "number": id, "cursor": cursor})
}

/// Every review thread with all its comments (nested comment pages followed per thread).
async fn review_threads(
    client: &GhClient,
    repo: &RepoRef,
    id: u64,
) -> Result<Vec<(Value, Vec<Value>)>, ProviderError> {
    let query = graphql::review_threads();
    let mut out = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let data = client.graphql(&query, vars(repo, id, &cursor)).await?;
        let conn = &data["repository"]["pullRequest"]["reviewThreads"];
        if conn.is_null() {
            return Err(ProviderError::NotFound);
        }
        for t in conn["nodes"].as_array().into_iter().flatten() {
            let mut comments = t["comments"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut page = t["comments"]["pageInfo"].clone();
            let thread_id = t["id"].as_str();
            let mut guard = 0;
            while let (Some(tid), true, Some(after)) = (
                thread_id,
                page["hasNextPage"].as_bool() == Some(true),
                page["endCursor"].as_str().map(str::to_string),
            ) {
                guard += 1;
                if guard > MAX_PAGES {
                    return Err(ProviderError::Decode("too many pages".into()));
                }
                let data = client
                    .graphql(
                        &graphql::thread_comments(),
                        json!({"id": tid, "cursor": after}),
                    )
                    .await?;
                let more = &data["node"]["comments"];
                comments.extend(more["nodes"].as_array().cloned().unwrap_or_default());
                page = more["pageInfo"].clone();
            }
            out.push((t.clone(), comments));
        }
        cursor = conn["pageInfo"]["endCursor"].as_str().map(str::to_string);
        if conn["pageInfo"]["hasNextPage"].as_bool() != Some(true) || cursor.is_none() {
            break;
        }
    }
    Ok(out)
}

async fn reviews(client: &GhClient, repo: &RepoRef, id: u64) -> Result<Vec<Value>, ProviderError> {
    let mut out = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let data = client
            .graphql(graphql::REVIEWS, vars(repo, id, &cursor))
            .await?;
        let conn = &data["repository"]["pullRequest"]["reviews"];
        out.extend(conn["nodes"].as_array().cloned().unwrap_or_default());
        cursor = conn["pageInfo"]["endCursor"].as_str().map(str::to_string);
        if conn["pageInfo"]["hasNextPage"].as_bool() != Some(true) || cursor.is_none() {
            break;
        }
    }
    Ok(out)
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

/// Where a thread sits. An outdated thread has no current `line`: it is placed by its
/// original line and flagged outdated. `subjectType: FILE` is file-level (no lines).
/// The anchor line's side is `diffSide`; a multi-line start keeps its own side.
fn thread_anchor(t: &Value) -> Option<Anchor> {
    let path = t.get("path").and_then(Value::as_str)?.to_string();
    let outdated = t["isOutdated"].as_bool().unwrap_or(false);
    let mut anchor = Anchor {
        path,
        outdated,
        ..Anchor::default()
    };
    if t["subjectType"].as_str() == Some("FILE") {
        return Some(anchor);
    }
    let (end, start) = if outdated {
        (line(t, "originalLine"), line(t, "originalStartLine"))
    } else {
        (
            line(t, "line").or(line(t, "originalLine")),
            line(t, "startLine").or(line(t, "originalStartLine")),
        )
    };
    let end_side = side(t, "diffSide").unwrap_or(Side::New);
    match end_side {
        Side::Old => anchor.from = end,
        Side::New => anchor.to = end,
    }
    let start_side = side(t, "startDiffSide").unwrap_or(end_side);
    anchor.start = start
        .filter(|&s| end.is_some_and(|e| s != e || start_side != end_side))
        .map(|s| (start_side, s));
    Some(anchor)
}

fn thread_comments(t: &Value, comments: &[Value], out: &mut CommentList) {
    let root = comments.first().and_then(|c| c["databaseId"].as_u64());
    let Some(thread) = t["id"].as_str().map(|s| super::write::thread_id(s, root)) else {
        for c in comments {
            out.skipped.push(SkippedComment {
                id: c["databaseId"].as_u64(),
                field: "thread.id".into(),
            });
        }
        return;
    };
    let anchor = thread_anchor(t);
    let can_resolve = t["viewerCanResolve"].as_bool() == Some(true)
        || t["viewerCanUnresolve"].as_bool() == Some(true);
    let resolved = t["isResolved"].as_bool().unwrap_or(false);
    for c in comments {
        let id = c["databaseId"]
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| c["id"].as_str().map(str::to_string));
        let Some(id) = id else {
            out.skipped.push(SkippedComment {
                id: None,
                field: "id".into(),
            });
            continue;
        };
        let (author, author_is_bot) = actor(c.get("author"));
        let created = time_at(c, "createdAt");
        let updated = time_at(c, "updatedAt");
        let created_on = created.or(updated).unwrap_or_default();
        let commit = field(c, "commit.oid")
            .or_else(|| field(c, "originalCommit.oid"))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.comments.push(Comment {
            id: CommentId(format!("rc:{id}")),
            thread: Some(thread.clone()),
            parent_id: None,
            can_resolve,
            author,
            body: c["body"].as_str().unwrap_or_default().to_string(),
            created_on,
            updated_on: updated.unwrap_or(created_on),
            deleted: false,
            resolved,
            anchor: anchor.clone().map(|a| Anchor { commit, ..a }),
            author_is_bot,
        });
    }
}

/// A REST issue comment: PR-level, flat (GitHub has no replies to them), not resolvable.
pub(crate) fn issue_comment(v: &Value) -> Option<Comment> {
    let id = v["id"].as_u64()?;
    let (author, author_is_bot) = actor(v.get("user"));
    let created = time_at(v, "created_at");
    let updated = time_at(v, "updated_at");
    let created_on = created.or(updated).unwrap_or_default();
    Some(Comment {
        id: CommentId(format!("ic:{id}")),
        thread: None,
        parent_id: None,
        can_resolve: false,
        author,
        body: v["body"].as_str().unwrap_or_default().to_string(),
        created_on,
        updated_on: updated.unwrap_or(created_on),
        deleted: false,
        resolved: false,
        anchor: None,
        author_is_bot,
    })
}

/// A submitted review's summary text as a read-only PR-level comment; None when the review
/// has no text.
fn review_body(v: &Value) -> Option<Comment> {
    let body = v["body"].as_str().filter(|b| !b.trim().is_empty())?;
    let id = v["databaseId"]
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| v["id"].as_str().map(str::to_string))?;
    let (author, author_is_bot) = actor(v.get("author"));
    let created_on = time_at(v, "submittedAt").unwrap_or_default();
    Some(Comment {
        id: CommentId(format!("rv:{id}")),
        thread: None,
        parent_id: None,
        can_resolve: false,
        author,
        body: body.to_string(),
        created_on,
        updated_on: time_at(v, "updatedAt").unwrap_or(created_on),
        deleted: false,
        resolved: false,
        anchor: None,
        author_is_bot,
    })
}
