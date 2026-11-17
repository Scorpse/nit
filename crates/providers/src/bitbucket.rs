//! Bitbucket Cloud implementation of [`Provider`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use prcore::comments::{
    looks_like_bot_name, Anchor, Comment, CommentId, NewComment, PrTask, ReviewAction, TaskState,
    ThreadId, UNKNOWN_AUTHOR,
};
use prcore::draft::Side;
use prcore::history::ReviewMark;
use prcore::model::{
    CheckState, DiffStat, Forge, PrState, PullRequest, RepoInfo, RepoRef, ReviewState, Reviewer,
    UserRef,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::bb_client::BbClient;
use crate::html_text::html_to_text;
use crate::provider::{
    Capabilities, CommentList, Discovered, PostRevision, PrList, Provider, ProviderError,
    ServerRevision, SkippedComment, SkippedPr,
};

mod mark;

/// Bitbucket caps pull request list pages at 50 entries.
const PR_PAGE_LEN: u32 = 50;

/// Partial response: add `participants` to the default list fields (`+` percent-encoded).
const PR_FIELDS: &str = "fields=%2Bvalues.participants";

pub struct BitbucketProvider {
    client: BbClient,
}

impl BitbucketProvider {
    pub fn new(client: BbClient) -> Self {
        BitbucketProvider { client }
    }

    /// The workspaces the account can see (slugs).
    pub async fn workspaces(&self) -> Result<Vec<String>, ProviderError> {
        self.client
            .get_all("/user/workspaces")
            .await?
            .into_iter()
            .map(|v| decode::<WorkspaceAccessDto>(v).map(|w| w.workspace.slug))
            .collect()
    }

    /// One workspace's repositories with name and visibility.
    pub async fn repo_infos(&self, workspace: &str) -> Result<Vec<RepoInfo>, ProviderError> {
        let mut out = Vec::new();
        for v in self
            .client
            .get_all(&format!("/repositories/{workspace}?role=member"))
            .await?
        {
            let dto: RepoDto = decode(v)?;
            if let Some(repo) = dto.full_name.as_deref().and_then(RepoRef::parse_bitbucket) {
                out.push(RepoInfo {
                    repo,
                    name: dto.name,
                    private: dto.is_private,
                });
            }
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct UserDto {
    uuid: String,
    display_name: String,
}

/// A user inside a PR entry: either part may be missing or null on real accounts.
#[derive(Deserialize)]
struct LooseUserDto {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceAccessDto {
    workspace: WorkspaceDto,
}

#[derive(Deserialize)]
struct WorkspaceDto {
    slug: String,
}

#[derive(Deserialize)]
struct RepoDto {
    full_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    is_private: Option<bool>,
}

#[derive(Deserialize)]
struct PrDto {
    id: u64,
    title: String,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    author: Option<LooseUserDto>,
    source: EndpointDto,
    destination: EndpointDto,
    #[serde(default)]
    comment_count: Option<u32>,
    #[serde(default)]
    task_count: Option<u32>,
    #[serde(default)]
    participants: Option<Vec<ParticipantDto>>,
    created_on: DateTime<Utc>,
    updated_on: DateTime<Utc>,
}

#[derive(Deserialize)]
struct EndpointDto {
    branch: BranchDto,
    /// Null when the branch was deleted.
    #[serde(default)]
    commit: Option<CommitDto>,
    #[serde(default)]
    repository: Option<EndpointRepoDto>,
}

#[derive(Deserialize)]
struct BranchDto {
    name: String,
}

#[derive(Deserialize)]
struct CommitDto {
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Deserialize)]
struct EndpointRepoDto {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct ParticipantDto {
    #[serde(default)]
    user: Option<LooseUserDto>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    approved: Option<bool>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct DiffStatEntryDto {
    #[serde(default)]
    lines_added: Option<u32>,
    #[serde(default)]
    lines_removed: Option<u32>,
}

// Comments and tasks are read field by field from JSON so that app, bot and integration
// entries with missing or odd fields still show: only an entry without an id is dropped.

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    field(v, path)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn time_at(v: &Value, path: &str) -> Option<DateTime<Utc>> {
    field(v, path).and_then(|t| DateTime::<Utc>::deserialize(t).ok())
}

/// A line number: a positive integer that fits `u32`, else `None`.
fn line_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .and_then(|n| u32::try_from(n).ok())
}

/// The author of a comment or task. Name: `display_name`, `nickname`, `name`, else
/// [`UNKNOWN_AUTHOR`]; id: `uuid`, `account_id`, else empty. Bot when the account is an app
/// user or team, or its display name or nickname looks like a tool (`looks_like_bot_name`).
fn author_of(user: Option<&Value>) -> (UserRef, bool) {
    let u = user.filter(|u| u.is_object()).unwrap_or(&Value::Null);
    let name = ["display_name", "nickname", "name"]
        .iter()
        .find_map(|k| str_at(u, k))
        .unwrap_or(UNKNOWN_AUTHOR);
    let uuid = str_at(u, "uuid").or_else(|| str_at(u, "account_id"));
    let bot = matches!(str_at(u, "type"), Some("app_user" | "team"))
        || ["display_name", "nickname"]
            .iter()
            .filter_map(|k| str_at(u, k))
            .any(looks_like_bot_name);
    let author = UserRef {
        id: uuid.unwrap_or_default().into(),
        display_name: name.trim().into(),
        login: None,
    };
    (author, bot)
}

/// `content.raw`; when it is empty, plain text from `content.html`. (`content.markup` names
/// the markup language, e.g. "markdown", so it is never body text.)
fn text_of(content: Option<&Value>) -> String {
    let c = content.unwrap_or(&Value::Null);
    if let Some(raw) = str_at(c, "raw") {
        return raw.into();
    }
    str_at(c, "html").map(html_to_text).unwrap_or_default()
}

/// Inline anchor. A ranged comment anchors on its end line (`to`/`from`) and keeps its
/// start for display; with only a start, the start is the anchor line. A line that is
/// present but not a positive integer makes the anchor file-level. No usable path: `None`.
/// `outdated` is read when it is a boolean (it is in Bitbucket's example responses, not in
/// the schema); anything else counts as not outdated.
fn anchor_of(inline: Option<&Value>) -> Option<Anchor> {
    let i = inline.filter(|i| i.is_object())?;
    let path = str_at(i, "path")?.to_string();
    let outdated = i.get("outdated").and_then(Value::as_bool).unwrap_or(false);
    let bad = ["from", "to"].iter().any(|k| {
        i.get(k)
            .is_some_and(|x| !x.is_null() && line_at(i, k).is_none())
    });
    if bad {
        return Some(Anchor {
            path,
            outdated,
            ..Anchor::default()
        });
    }
    let (start_from, start_to) = (line_at(i, "start_from"), line_at(i, "start_to"));
    let (mut from, mut to) = (line_at(i, "from"), line_at(i, "to"));
    if from.is_none() && to.is_none() {
        (from, to) = (start_from, start_to);
    }
    let (end, start, side) = if to.is_some() {
        (to, start_to, Side::New)
    } else {
        (from, start_from, Side::Old)
    };
    Some(Anchor {
        path,
        from,
        to,
        start: start
            .filter(|&s| end.is_some_and(|e| s < e))
            .map(|l| (side, l)),
        outdated,
        old_path: None,
        commit: None,
    })
}

/// Decodes one comment entry; `None` only when it has no usable id.
fn comment_of(v: &Value) -> Option<Comment> {
    let id = v.get("id").and_then(Value::as_u64)?;
    let (author, author_is_bot) = author_of(v.get("user"));
    let created = time_at(v, "created_on");
    let updated = time_at(v, "updated_on");
    // A missing timestamp takes the other one, else the epoch (sorts first).
    let created_on = created.or(updated).unwrap_or_default();
    Some(Comment {
        id: id.into(),
        // Bitbucket replies (and resolves) address any comment by its own id; replies are
        // grouped by the parent walk in `build_threads`.
        thread: Some(ThreadId::from(id)),
        parent_id: field(v, "parent.id")
            .and_then(Value::as_u64)
            .map(CommentId::from),
        can_resolve: true,
        author,
        body: text_of(v.get("content")),
        created_on,
        updated_on: updated.unwrap_or(created_on),
        deleted: v.get("deleted").and_then(Value::as_bool).unwrap_or(false),
        resolved: v.get("resolution").is_some_and(|r| !r.is_null()),
        anchor: anchor_of(v.get("inline")),
        author_is_bot,
    })
}

/// Decodes one task entry; `None` without an id or for an unpublished (pending) draft.
fn task_of(v: &Value) -> Option<PrTask> {
    let id = v.get("id").and_then(Value::as_u64)?;
    if v.get("pending").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(PrTask {
        id,
        state: match str_at(v, "state") {
            Some("RESOLVED") => TaskState::Resolved,
            _ => TaskState::Open,
        },
        content: text_of(v.get("content")),
        creator: author_of(v.get("creator")).0,
        comment_id: field(v, "comment.id")
            .and_then(Value::as_u64)
            .map(CommentId::from),
    })
}

/// Bitbucket thread ids are comment numbers; anything else was not made by this provider.
fn bitbucket_comment_number(t: &ThreadId) -> Result<u64, ProviderError> {
    t.as_u64()
        .ok_or_else(|| ProviderError::Invalid("not a Bitbucket comment id".into()))
}

fn comment_body(c: &NewComment) -> Result<Value, ProviderError> {
    use serde_json::json;
    Ok(match c {
        NewComment::Pr { text } => json!({"content":{"raw":text}}),
        NewComment::File { path, text } => json!({"content":{"raw":text},"inline":{"path":path}}),
        NewComment::Inline {
            path,
            side,
            line,
            start,
            text,
        } => {
            if start.is_some() {
                // Bitbucket has no multi-line comments: never post a range as one line.
                return Err(ProviderError::Invalid(
                    "multi-line comments are not supported on Bitbucket".into(),
                ));
            }
            // Exactly one of from/to: `to` is the new-file line, `from` the old-file line.
            let key = match side {
                Side::New => "to",
                Side::Old => "from",
            };
            json!({"content":{"raw":text},"inline":{"path":path,key:line}})
        }
        NewComment::Reply { thread, text } => {
            let parent_id = bitbucket_comment_number(thread)?;
            json!({"content":{"raw":text},"parent":{"id":parent_id}})
        }
    })
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|e| ProviderError::Decode(e.to_string()))
}

impl From<UserDto> for UserRef {
    fn from(u: UserDto) -> Self {
        UserRef {
            id: u.uuid,
            display_name: u.display_name,
            login: None,
        }
    }
}

impl From<LooseUserDto> for UserRef {
    fn from(u: LooseUserDto) -> Self {
        UserRef {
            id: u.uuid.unwrap_or_default(),
            display_name: u.display_name.unwrap_or_else(|| "unknown".into()),
            login: None,
        }
    }
}

fn commit_hash(c: Option<CommitDto>) -> String {
    c.and_then(|c| c.hash).unwrap_or_default()
}

type Check = fn(&Value) -> bool;

fn is_str(v: &Value) -> bool {
    v.is_string()
}
fn opt_str(v: &Value) -> bool {
    v.is_null() || v.is_string()
}
fn opt_bool(v: &Value) -> bool {
    v.is_null() || v.is_boolean()
}
fn opt_u32(v: &Value) -> bool {
    v.is_null() || v.as_u64().is_some_and(|n| n <= u64::from(u32::MAX))
}
fn opt_obj(v: &Value) -> bool {
    v.is_null() || v.is_object()
}
fn opt_array(v: &Value) -> bool {
    v.is_null() || v.is_array()
}
fn is_time(v: &Value) -> bool {
    DateTime::<Utc>::deserialize(v).is_ok()
}

fn field<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |node, key| node.get(key))
}

/// Path of the first field that makes a PR entry undecodable. Only paths are returned,
/// never values, so the result is safe to store and show.
fn bad_pr_field(v: &Value) -> String {
    let required: [(&str, Check); 6] = [
        ("id", |v| v.as_u64().is_some()),
        ("title", is_str),
        ("source.branch.name", is_str),
        ("destination.branch.name", is_str),
        ("created_on", is_time),
        ("updated_on", is_time),
    ];
    for (path, ok) in required {
        if !field(v, path).is_some_and(ok) {
            return path.into();
        }
    }
    // Optional fields may be missing or null, but not of the wrong type.
    let optional: [(&str, Check); 15] = [
        ("draft", opt_bool),
        ("comment_count", opt_u32),
        ("task_count", opt_u32),
        ("author", opt_obj),
        ("author.uuid", opt_str),
        ("author.display_name", opt_str),
        ("source.commit", opt_obj),
        ("source.commit.hash", opt_str),
        ("source.repository", opt_obj),
        ("source.repository.full_name", opt_str),
        ("destination.commit", opt_obj),
        ("destination.commit.hash", opt_str),
        ("destination.repository", opt_obj),
        ("destination.repository.full_name", opt_str),
        ("participants", opt_array),
    ];
    for (path, ok) in optional {
        if !field(v, path).is_none_or(ok) {
            return path.into();
        }
    }
    let participant: [(&str, Check); 6] = [
        ("user", opt_obj),
        ("user.uuid", opt_str),
        ("user.display_name", opt_str),
        ("role", opt_str),
        ("approved", opt_bool),
        ("state", opt_str),
    ];
    for (i, p) in v["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !p.is_object() {
            return format!("participants[{i}]");
        }
        for (path, ok) in participant {
            if !field(p, path).is_none_or(ok) {
                return format!("participants[{i}].{path}");
            }
        }
    }
    "unknown".into()
}

fn review_state(p: &ParticipantDto) -> ReviewState {
    match p.state.as_deref() {
        Some("changes_requested") => ReviewState::ChangesRequested,
        Some("approved") => ReviewState::Approved,
        _ if p.approved == Some(true) => ReviewState::Approved,
        _ => ReviewState::Pending,
    }
}

fn state_param(state: &PrState) -> &'static str {
    match state {
        PrState::Open => "OPEN",
        PrState::Merged => "MERGED",
        PrState::Declined => "DECLINED",
        PrState::Superseded => "SUPERSEDED",
    }
}

impl PrDto {
    fn into_pr(self, repo: &RepoRef, state: &PrState) -> PullRequest {
        // A missing repository (deleted fork) must not look like the destination repo.
        let source_repo = self
            .source
            .repository
            .and_then(|r| r.full_name)
            .and_then(|n| RepoRef::parse_bitbucket(&n))
            .unwrap_or_else(|| RepoRef {
                host: prcore::model::BITBUCKET_HOST.into(),
                namespace: "unknown".into(),
                slug: "unknown".into(),
            });
        let reviewers = self
            .participants
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                let state = review_state(&p);
                // Anyone who approved or asked for changes counts, even when never added
                // as a reviewer; other non-reviewers are just participants.
                if p.role.as_deref() != Some("REVIEWER") && state == ReviewState::Pending {
                    return None;
                }
                p.user.map(|u| Reviewer {
                    user: u.into(),
                    state,
                })
            })
            .collect();
        PullRequest {
            repo: repo.clone(),
            id: self.id,
            title: self.title,
            state: state.clone(),
            draft: self.draft.unwrap_or(false),
            author: self.author.map_or_else(
                || UserRef {
                    id: String::new(),
                    display_name: "unknown".into(),
                    login: None,
                },
                UserRef::from,
            ),
            source_branch: self.source.branch.name,
            dest_branch: self.destination.branch.name,
            source_commit: commit_hash(self.source.commit),
            dest_commit: commit_hash(self.destination.commit),
            source_repo,
            comment_count: self.comment_count.unwrap_or(0),
            task_count: self.task_count.unwrap_or(0),
            reviewers,
            created_on: self.created_on,
            updated_on: self.updated_on,
            forge: prcore::model::Forge::BitbucketCloud,
            account: Default::default(),
            web_url: String::new(),
            node_id: None,
            review_requested_from_me: false,
            locked: false,
            checks: Default::default(),
            has_conflicts: false,
            new_commits: false,
        }
    }
}

/// Keeps the partial-response parameter on `next` links that do not echo it.
fn with_pr_fields(next: String) -> String {
    let query = next.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query
        .split('&')
        .any(|kv| kv.split('=').next() == Some("fields"))
    {
        return next;
    }
    let sep = if next.contains('?') { '&' } else { '?' };
    format!("{next}{sep}{PR_FIELDS}")
}

/// Build results fetched at once: enough to keep a big repository's sync quick, few enough
/// not to trip Bitbucket's rate limit.
const STATUS_CONCURRENCY: usize = 6;

/// Folds the commit statuses of a PR: any failed or stopped build fails it, else any running
/// build keeps it running, else any success passes it; no statuses is unknown.
fn fold_statuses(values: &[Value]) -> CheckState {
    let mut out = CheckState::Unknown;
    for s in values {
        match str_at(s, "state") {
            Some("FAILED" | "STOPPED") => return CheckState::Failing,
            Some("INPROGRESS") => out = CheckState::Running,
            Some("SUCCESSFUL") if out == CheckState::Unknown => out = CheckState::Passing,
            _ => {}
        }
    }
    out
}

impl BitbucketProvider {
    /// Sets `checks` on each PR from its commit statuses. Best effort: a PR whose statuses
    /// cannot be read keeps `Unknown`, and the list never fails because of it.
    async fn fill_checks(&self, repo: &RepoRef, prs: &mut [PullRequest]) {
        use futures_util::stream::{self, StreamExt};
        let ids: Vec<u64> = prs.iter().map(|p| p.id).collect();
        let results: Vec<(u64, CheckState)> = stream::iter(ids)
            .map(|id| async move {
                let url = format!(
                    "/repositories/{}/{}/pullrequests/{id}/statuses?pagelen=100",
                    repo.namespace, repo.slug
                );
                match self.client.get_page(&url).await {
                    Ok((values, _)) => (id, fold_statuses(&values)),
                    Err(_) => (id, CheckState::Unknown),
                }
            })
            .buffer_unordered(STATUS_CONCURRENCY)
            .collect()
            .await;
        for (id, state) in results {
            if let Some(p) = prs.iter_mut().find(|p| p.id == id) {
                p.checks = state;
            }
        }
    }

    /// Walks a PR list (`first_page_only` stops after one page), skipping undecodable entries.
    async fn collect_prs(
        &self,
        url: String,
        repo: &RepoRef,
        state: &PrState,
        limit: usize,
        first_page_only: bool,
    ) -> Result<PrList, ProviderError> {
        let mut out = PrList::default();
        let mut next = Some(url);
        while let Some(u) = next.take() {
            let (values, n) = self.client.get_page(&u).await?;
            for v in values {
                // One undecodable entry must not fail the whole list: skip it and say why.
                match serde_json::from_value::<PrDto>(v.clone()) {
                    Ok(dto) => out.prs.push(dto.into_pr(repo, state)),
                    Err(_) => out.skipped.push(SkippedPr {
                        id: v.get("id").and_then(Value::as_u64),
                        field: bad_pr_field(&v),
                    }),
                }
                if out.prs.len() >= limit {
                    return Ok(out);
                }
            }
            if first_page_only {
                break;
            }
            next = n.map(with_pr_fields);
        }
        Ok(out)
    }
}

/// Escapes `\` and `"` for use inside a double-quoted Bitbucket query string.
fn escape_q_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[async_trait]
impl Provider for BitbucketProvider {
    fn forge(&self) -> Forge {
        Forge::BitbucketCloud
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::BITBUCKET
    }

    async fn me(&self) -> Result<UserRef, ProviderError> {
        let user: UserDto = decode(self.client.get_json("/user").await?)?;
        Ok(user.into())
    }

    /// The workspaces, then each workspace's repositories. Auth anywhere is `Err(Auth)`
    /// (the account is unusable); another failure of one workspace is reported in
    /// `failed` and the others are still listed.
    async fn discover(&self) -> Result<Discovered, ProviderError> {
        let namespaces = self.workspaces().await?;
        let mut out = Discovered::default();
        for ws in &namespaces {
            match self.repo_infos(ws).await {
                Ok(r) => out.repos.extend(r),
                Err(ProviderError::Auth) => return Err(ProviderError::Auth),
                Err(e) => out.failed.push((ws.clone(), e)),
            }
        }
        out.namespaces = namespaces;
        Ok(out)
    }

    async fn revision(&self, repo: &RepoRef, id: u64) -> Result<ServerRevision, ProviderError> {
        // Comments anchor to the current diff: head and destination are all that is
        // compared; Bitbucket gives no base.
        let pr = self.pr(repo, id).await?;
        Ok(ServerRevision {
            head: pr.source_commit,
            base: None,
            start: None,
            dest_branch: pr.dest_branch,
        })
    }

    async fn prs(
        &self,
        repo: &RepoRef,
        state: PrState,
        max: usize,
    ) -> Result<PrList, ProviderError> {
        // Open PRs are always collected in full; other states stop at `max`.
        let limit = if state == PrState::Open {
            usize::MAX
        } else {
            max
        };
        if limit == 0 {
            return Ok(PrList::default());
        }
        let mut url = format!(
            "/repositories/{}/{}/pullrequests?state={}&pagelen={PR_PAGE_LEN}&{PR_FIELDS}",
            repo.namespace,
            repo.slug,
            state_param(&state)
        );
        if state == PrState::Merged {
            url.push_str("&sort=-updated_on");
        }
        let mut list = self.collect_prs(url, repo, &state, limit, false).await?;
        if state == PrState::Open {
            self.fill_checks(repo, &mut list.prs).await;
        }
        Ok(list)
    }

    async fn merged_by_source_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<PrList, ProviderError> {
        let q = format!(
            "source.branch.name=\"{}\" AND state=\"MERGED\"",
            escape_q_string(branch)
        );
        let url = format!(
            "/repositories/{}/{}/pullrequests?q={}&sort=-updated_on&pagelen={PR_PAGE_LEN}&{PR_FIELDS}",
            repo.namespace,
            repo.slug,
            percent_encode(&q)
        );
        self.collect_prs(url, repo, &PrState::Merged, usize::MAX, true)
            .await
    }

    async fn pr(&self, repo: &RepoRef, id: u64) -> Result<PullRequest, ProviderError> {
        let v = self
            .client
            .get_json(&format!(
                "/repositories/{}/{}/pullrequests/{id}",
                repo.namespace, repo.slug
            ))
            .await?;
        let state = match v.get("state").and_then(Value::as_str) {
            Some("MERGED") => PrState::Merged,
            Some("DECLINED") => PrState::Declined,
            Some("SUPERSEDED") => PrState::Superseded,
            _ => PrState::Open,
        };
        Ok(decode::<PrDto>(v)?.into_pr(repo, &state))
    }

    async fn my_last_review(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> Result<Option<ReviewMark>, ProviderError> {
        let me = self.me().await?;
        let v = self
            .client
            .get_json(&format!(
                "/repositories/{}/{}/pullrequests/{id}",
                repo.namespace, repo.slug
            ))
            .await?;
        Ok(mark::mark_of(&v, &me.id))
    }

    async fn comments(&self, repo: &RepoRef, id: u64) -> Result<CommentList, ProviderError> {
        // Every page (pagelen 100, `next` links on the same origin only), oldest first, from
        // every author (people, apps, bots), resolved and deleted ones included.
        let path = format!(
            "/repositories/{}/{}/pullrequests/{id}/comments?sort=created_on",
            repo.namespace, repo.slug
        );
        let (values, tasks) = tokio::join!(self.client.get_all(&path), self.tasks(repo, id));
        let mut out = CommentList {
            // Tasks are extra: when they cannot be read, the comments still load.
            tasks: tasks.ok(),
            ..CommentList::default()
        };
        for v in values? {
            // Unpublished drafts of the author are not comments yet.
            if v.get("pending").and_then(Value::as_bool) == Some(true) {
                out.skipped.push(SkippedComment {
                    id: v.get("id").and_then(Value::as_u64),
                    field: "pending".into(),
                });
                continue;
            }
            // Only an entry without an id is skipped; anything else incomplete still shows.
            match comment_of(&v) {
                Some(c) => out.comments.push(c),
                None => out.skipped.push(SkippedComment {
                    id: None,
                    field: "id".into(),
                }),
            }
        }
        Ok(out)
    }

    async fn tasks(&self, repo: &RepoRef, id: u64) -> Result<Vec<PrTask>, ProviderError> {
        let values = self
            .client
            .get_all(&format!(
                "/repositories/{}/{}/pullrequests/{id}/tasks",
                repo.namespace, repo.slug
            ))
            .await?;
        Ok(values.iter().filter_map(task_of).collect())
    }

    async fn create_task(
        &self,
        repo: &RepoRef,
        id: u64,
        content: &str,
    ) -> Result<PrTask, ProviderError> {
        let v = self
            .client
            .post_create(
                &format!(
                    "/repositories/{}/{}/pullrequests/{id}/tasks",
                    repo.namespace, repo.slug
                ),
                &serde_json::json!({ "content": { "raw": content } }),
            )
            .await?;
        // The write succeeded; an unreadable echo must not look like a failed post.
        task_of(&v).ok_or(ProviderError::PostedUnreadable)
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        id: u64,
        // Bitbucket anchors to the current diff; `inline_allowed` checked the revision
        _at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, ProviderError> {
        if matches!(c, NewComment::Inline { line: 0, .. }) {
            return Err(ProviderError::Invalid(
                "inline line numbers start at 1".into(),
            ));
        }
        let v = self
            .client
            .post_create(
                &format!(
                    "/repositories/{}/{}/pullrequests/{id}/comments",
                    repo.namespace, repo.slug
                ),
                &comment_body(c)?,
            )
            .await?;
        // The write succeeded; an unreadable echo must not look like a failed post.
        comment_of(&v).ok_or(ProviderError::PostedUnreadable)
    }

    async fn set_resolved(
        &self,
        repo: &RepoRef,
        id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), ProviderError> {
        let comment_id = bitbucket_comment_number(thread)?;
        let path = format!(
            "/repositories/{}/{}/pullrequests/{id}/comments/{comment_id}/resolve",
            repo.namespace, repo.slug
        );
        if resolved {
            self.client.post_checked(&path, None).await?;
        } else {
            self.client.delete_checked(&path).await?;
        }
        Ok(())
    }

    async fn set_review(
        &self,
        repo: &RepoRef,
        id: u64,
        a: ReviewAction,
        // Bitbucket reviews carry no text
        _body: Option<&str>,
    ) -> Result<(), ProviderError> {
        let base = format!(
            "/repositories/{}/{}/pullrequests/{id}",
            repo.namespace, repo.slug
        );
        match a {
            ReviewAction::Approve => {
                self.client
                    .post_checked(&format!("{base}/approve"), None)
                    .await?
            }
            ReviewAction::RequestChanges => {
                self.client
                    .post_checked(&format!("{base}/request-changes"), None)
                    .await?
            }
            ReviewAction::ClearApproval => {
                self.client
                    .delete_checked(&format!("{base}/approve"))
                    .await?
            }
            ReviewAction::ClearRequestChanges => {
                self.client
                    .delete_checked(&format!("{base}/request-changes"))
                    .await?
            }
        };
        Ok(())
    }

    async fn diffstat(&self, repo: &RepoRef, id: u64) -> Result<DiffStat, ProviderError> {
        let entries = self
            .client
            .get_all(&format!(
                "/repositories/{}/{}/pullrequests/{id}/diffstat",
                repo.namespace, repo.slug
            ))
            .await?;
        let mut stat = DiffStat {
            files: 0,
            lines_added: 0,
            lines_removed: 0,
            max_file_lines: 0,
        };
        for v in entries {
            let e: DiffStatEntryDto = decode(v)?;
            let (a, r) = (e.lines_added.unwrap_or(0), e.lines_removed.unwrap_or(0));
            stat.files += 1;
            stat.lines_added += a;
            stat.lines_removed += r;
            stat.max_file_lines = stat.max_file_lines.max(a + r);
        }
        Ok(stat)
    }
}

#[cfg(test)]
mod checks_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_fold_to_one_state() {
        let s = |states: &[&str]| -> Vec<Value> {
            states.iter().map(|s| json!({"state": s})).collect()
        };
        assert_eq!(fold_statuses(&[]), CheckState::Unknown);
        assert_eq!(
            fold_statuses(&s(&["SUCCESSFUL", "SUCCESSFUL"])),
            CheckState::Passing
        );
        assert_eq!(
            fold_statuses(&s(&["SUCCESSFUL", "INPROGRESS"])),
            CheckState::Running
        );
        assert_eq!(
            fold_statuses(&s(&["INPROGRESS", "FAILED"])),
            CheckState::Failing
        );
        assert_eq!(fold_statuses(&s(&["STOPPED"])), CheckState::Failing);
    }
}
