//! GitHub (github.com, GitHub Enterprise Server, GHE.com) implementation of [`Provider`].
//! Writes (spec 12, P2) are in `write`; inline posting stays gated by
//! `INLINE_ENABLED_GITHUB` in the app.

mod bases;
mod client;
mod comments;
pub mod graphql;
mod inbox;
mod write;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use prcore::comments::{AnchorModel, Comment, NewComment, PrTask, ReviewAction, ThreadId};
use prcore::model::{DiffStat, Forge, PrState, PullRequest, RepoInfo, RepoRef, UserRef};
use reqwest::Method;
use serde_json::{json, Value};

pub use bases::{bases_for, GitHubBases};
pub use client::{
    next_link, Conditional, GhClient, API_VERSION, DEFAULT_PAUSE, MAX_INLINE_WAIT,
    MIN_WRITE_SPACING, SSO_HINT,
};
pub use inbox::Teams;
pub use write::INLINE_ENABLED_GITHUB;

use crate::provider::{
    Capabilities, CommentList, Discovered, PostRevision, PrList, Provider, ProviderError,
    ServerRevision, SuggestionSyntax,
};

/// What GitHub supports (spec 4.3). Clearing a review does not exist on GitHub, and issue
/// comments take no replies.
pub const CAPABILITIES: Capabilities = Capabilities {
    request_changes: true,
    clear_approval: false,
    clear_request_changes: false,
    resolve_threads: true,
    reply_to_general_comments: false,
    file_comments: true,
    multi_line_comments: true,
    suggestions: Some(SuggestionSyntax::GitHubFence),
    tasks: false,
    inline_anchor: AnchorModel::HeadCommit,
    batch_review: false,
    read_only: false,
};

/// Per repository: the ETag of the last REST open-PR list and what it produced.
type ListCache = HashMap<String, (String, PrList)>;

/// The search behind the "requested elsewhere" hint (spec 8.1, decision D4): open PRs in
/// unarchived repositories that request my review. One page of 100 (the hint names
/// repositories; the search API caps results at 1,000 anyway).
pub const REVIEW_REQUESTED_SEARCH: &str =
    "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3A%40me+archived%3Afalse&per_page=100";

/// At most one "requested elsewhere" search per account in this interval (the search API
/// allows 30 requests a minute; the app asks at most once per sync).
pub const SEARCH_EVERY: Duration = Duration::from_secs(60);

/// Lets a call through at most once per `every`.
#[derive(Debug)]
pub struct MinInterval {
    every: Duration,
    last: Mutex<Option<Instant>>,
}

impl MinInterval {
    pub fn new(every: Duration) -> MinInterval {
        MinInterval {
            every,
            last: Mutex::new(None),
        }
    }

    /// True (and `now` recorded) when no call went through in the `every` before `now`.
    pub fn try_at(&self, now: Instant) -> bool {
        let mut last = lock(&self.last);
        match *last {
            Some(t) if now.saturating_duration_since(t) < self.every => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// One GitHub account.
pub struct GitHubProvider {
    client: GhClient,
    /// The account's git host ("github.com", "ghe.corp.example"): the `RepoRef` host.
    host: String,
    me: Mutex<Option<UserRef>>,
    teams: Mutex<Option<Teams>>,
    lists: Mutex<ListCache>,
    /// Sizes from the GraphQL list: (repo key, number) -> (head oid, size).
    stats: Mutex<HashMap<(String, u64), (String, DiffStat)>>,
    /// Bounds the "requested elsewhere" search (`SEARCH_EVERY`).
    search_gate: MinInterval,
}

impl std::fmt::Debug for GitHubProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubProvider")
            .field("client", &self.client)
            .field("host", &self.host)
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Percent-encodes a path segment (owner, repo, branch).
fn enc(s: &str) -> String {
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

impl GitHubProvider {
    /// `api_base` and `graphql_url` per spec 5.1 (same origin); `host` is the git host.
    pub fn new(api_base: &str, graphql_url: &str, host: &str, token: &str) -> GitHubProvider {
        GitHubProvider {
            client: GhClient::new(api_base, graphql_url, token),
            host: host.trim_end_matches('/').to_ascii_lowercase(),
            me: Mutex::new(None),
            teams: Mutex::new(None),
            lists: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            search_gate: MinInterval::new(SEARCH_EVERY),
        }
    }

    /// The HTTP client (rate-limit state, request counter, SSO link, token kind).
    pub fn client(&self) -> &GhClient {
        &self.client
    }

    fn repo_path(repo: &RepoRef) -> String {
        format!("/repos/{}/{}", enc(&repo.namespace), enc(&repo.slug))
    }

    /// `me()`, asked once per provider.
    async fn me_cached(&self) -> Result<UserRef, ProviderError> {
        if let Some(me) = lock(&self.me).clone() {
            return Ok(me);
        }
        let me = self.me().await?;
        *lock(&self.me) = Some(me.clone());
        Ok(me)
    }

    /// The user's teams, asked once and only when a PR requests a team. A token that may
    /// not read them (no `read:org`) gets an empty list: team requests then do not count
    /// as "needs me" (UNVERIFIED for fine-grained tokens).
    async fn teams_cached(&self) -> Result<Teams, ProviderError> {
        if let Some(t) = lock(&self.teams).clone() {
            return Ok(t);
        }
        let teams = match self.client.get_paged("/user/teams?per_page=100").await {
            Ok(list) => list
                .iter()
                .filter_map(|t| {
                    let org = t["organization"]["login"].as_str()?;
                    let slug = t["slug"].as_str()?;
                    Some((org.to_ascii_lowercase(), slug.to_ascii_lowercase()))
                })
                .collect(),
            Err(e @ (ProviderError::Auth | ProviderError::RateLimited)) => return Err(e),
            Err(_) => Vec::new(),
        };
        *lock(&self.teams) = Some(teams.clone());
        Ok(teams)
    }

    /// Open PRs: the REST list with `If-None-Match` is the gate (a 304 costs nothing and
    /// returns the last result); on a change, the GraphQL query brings sizes and reviews.
    async fn open_prs(&self, repo: &RepoRef) -> Result<PrList, ProviderError> {
        let key = repo.key();
        let cached = lock(&self.lists).get(&key).cloned();
        let path = format!(
            "{}/pulls?state=open&sort=updated&direction=desc&per_page=100",
            Self::repo_path(repo)
        );
        let etag = cached.as_ref().map(|(e, _)| e.as_str());
        let (body, etag) = match self.client.get_conditional(&path, etag).await? {
            Conditional::NotModified => match cached {
                Some((_, list)) => return Ok(list),
                None => (Value::Null, None),
            },
            Conditional::Fresh { body, etag } => (body, etag),
        };
        let list = if body.as_array().is_some_and(Vec::is_empty) {
            PrList::default()
        } else {
            self.open_prs_graphql(repo).await?
        };
        if let Some(e) = etag {
            lock(&self.lists).insert(key, (e, list.clone()));
        }
        Ok(list)
    }

    async fn open_prs_graphql(&self, repo: &RepoRef) -> Result<PrList, ProviderError> {
        let me = self.me_cached().await?;
        let mut nodes: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..client::MAX_PAGES {
            let data = self
                .client
                .graphql(
                    graphql::OPEN_PRS,
                    json!({"owner": repo.namespace, "name": repo.slug, "cursor": cursor}),
                )
                .await?;
            let conn = &data["repository"]["pullRequests"];
            if conn.is_null() {
                return Err(ProviderError::NotFound);
            }
            nodes.extend(conn["nodes"].as_array().cloned().unwrap_or_default());
            cursor = conn["pageInfo"]["endCursor"].as_str().map(str::to_string);
            if conn["pageInfo"]["hasNextPage"].as_bool() != Some(true) || cursor.is_none() {
                break;
            }
        }
        let wants_teams = nodes.iter().any(|n| {
            n["reviewRequests"]["nodes"].as_array().is_some_and(|r| {
                r.iter()
                    .any(|x| x["requestedReviewer"]["__typename"] == "Team")
            })
        });
        let teams = if wants_teams {
            self.teams_cached().await?
        } else {
            Vec::new()
        };
        let mut out = PrList::default();
        let mut stats = lock(&self.stats);
        for n in &nodes {
            match inbox::pr_from_graphql(n, repo, Some(&me), &teams) {
                Ok((pr, stat)) => {
                    if let Some(s) = stat {
                        stats.insert((repo.key(), pr.id), (pr.source_commit.clone(), s));
                    }
                    out.prs.push(pr);
                }
                Err(s) => out.skipped.push(s),
            }
        }
        Ok(out)
    }

    /// One page of closed PRs (REST), keeping those in state `keep` (closed with
    /// `merged_at` is Merged, without it Declined).
    async fn closed_list(
        &self,
        repo: &RepoRef,
        path: &str,
        keep: PrState,
    ) -> Result<PrList, ProviderError> {
        let v = self.client.get_json(path).await?;
        let Some(items) = v.as_array() else {
            return Err(ProviderError::Decode("expected a list".into()));
        };
        let mut out = PrList::default();
        for item in items {
            match inbox::pr_from_rest(item, repo) {
                Ok(pr) if pr.state == keep => out.prs.push(pr),
                Ok(_) => {}
                Err(s) => out.skipped.push(s),
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Provider for GitHubProvider {
    fn forge(&self) -> Forge {
        Forge::GitHub
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn paused_until(&self) -> Option<std::time::SystemTime> {
        self.client.paused_until()
    }

    async fn me(&self) -> Result<UserRef, ProviderError> {
        let v = self.client.get_json("/user").await?;
        if v["id"].as_u64().is_none() || v["login"].as_str().is_none() {
            return Err(ProviderError::Decode("user without id or login".into()));
        }
        Ok(inbox::actor(Some(&v)).0)
    }

    /// `REVIEW_REQUESTED_SEARCH`, at most once per `SEARCH_EVERY` (a call in between sends
    /// nothing and answers `None`; a failed search counts too). Each result's repository
    /// comes from its `repository_url` (`.../repos/{owner}/{name}`), on this account's host.
    async fn review_requests_elsewhere(&self) -> Result<Option<Vec<RepoRef>>, ProviderError> {
        if !self.search_gate.try_at(Instant::now()) {
            return Ok(None);
        }
        let v = self.client.get_json(REVIEW_REQUESTED_SEARCH).await?;
        let items = v["items"]
            .as_array()
            .ok_or_else(|| ProviderError::Decode("search without items".into()))?;
        let repos = items
            .iter()
            .filter_map(|it| {
                let url = it["repository_url"].as_str()?;
                let mut seg = url.trim_end_matches('/').rsplit('/');
                let (name, owner) = (seg.next()?, seg.next()?);
                RepoRef::parse(&self.host, &format!("{owner}/{name}"))
            })
            .collect();
        Ok(Some(repos))
    }

    /// `GET /user/repos` (owner, collaborator and organization member), every page. One
    /// listing: an error is `Err` (nothing could be listed), never a failed namespace.
    async fn discover(&self) -> Result<Discovered, ProviderError> {
        let items = self
            .client
            .get_paged(
                "/user/repos?per_page=100&affiliation=owner,collaborator,organization_member&sort=full_name",
            )
            .await?;
        let mut out = Discovered::default();
        for v in &items {
            let Some(repo) = v["full_name"]
                .as_str()
                .and_then(|n| RepoRef::parse(&self.host, n))
                .filter(|r| !r.namespace.contains('/'))
            else {
                continue;
            };
            if !out.namespaces.contains(&repo.namespace) {
                out.namespaces.push(repo.namespace.clone());
            }
            out.repos.push(RepoInfo {
                repo,
                name: v["name"].as_str().map(str::to_string),
                private: v["private"].as_bool(),
            });
        }
        out.namespaces.sort();
        Ok(out)
    }

    async fn prs(
        &self,
        repo: &RepoRef,
        state: PrState,
        max: usize,
    ) -> Result<PrList, ProviderError> {
        match state {
            PrState::Open => self.open_prs(repo).await,
            PrState::Superseded => Ok(PrList::default()),
            _ if max == 0 => Ok(PrList::default()),
            closed => {
                let path = format!(
                    "{}/pulls?state=closed&sort=updated&direction=desc&per_page={}",
                    Self::repo_path(repo),
                    max.min(100)
                );
                self.closed_list(repo, &path, closed).await
            }
        }
    }

    async fn merged_by_source_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<PrList, ProviderError> {
        let path = format!(
            "{}/pulls?state=closed&head={}%3A{}&sort=updated&direction=desc&per_page=100",
            Self::repo_path(repo),
            enc(&repo.namespace),
            enc(branch)
        );
        self.closed_list(repo, &path, PrState::Merged).await
    }

    async fn diffstat(&self, repo: &RepoRef, id: u64) -> Result<DiffStat, ProviderError> {
        if let Some((_, s)) = lock(&self.stats).get(&(repo.key(), id)) {
            return Ok(s.clone());
        }
        let v = self
            .client
            .get_json(&format!("{}/pulls/{id}", Self::repo_path(repo)))
            .await?;
        inbox::stat_from_rest(&v).ok_or_else(|| ProviderError::Decode("no size fields".into()))
    }

    async fn pr(&self, repo: &RepoRef, id: u64) -> Result<PullRequest, ProviderError> {
        let v = self
            .client
            .get_json(&format!("{}/pulls/{id}", Self::repo_path(repo)))
            .await?;
        inbox::pr_from_rest(&v, repo)
            .map_err(|s| ProviderError::Decode(format!("field {}", s.field)))
    }

    /// Head and base branch as the server has them; GitHub reports no merge base here.
    async fn revision(&self, repo: &RepoRef, id: u64) -> Result<ServerRevision, ProviderError> {
        let pr = self.pr(repo, id).await?;
        Ok(ServerRevision {
            head: pr.source_commit,
            base: None,
            start: None,
            dest_branch: pr.dest_branch,
        })
    }

    async fn comments(&self, repo: &RepoRef, id: u64) -> Result<CommentList, ProviderError> {
        comments::comments(&self.client, repo, id).await
    }

    async fn tasks(&self, _repo: &RepoRef, _id: u64) -> Result<Vec<PrTask>, ProviderError> {
        Err(ProviderError::Invalid("GitHub has no PR tasks".into()))
    }

    /// Sends `at.head` as `commit_id` for file and inline comments (the app checks the head
    /// and, for inline, `INLINE_ENABLED_GITHUB` before calling).
    async fn post_comment(
        &self,
        repo: &RepoRef,
        id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, ProviderError> {
        let (path, body) = write::comment_request(&Self::repo_path(repo), id, at, c)?;
        let v = self.client.write(Method::POST, &path, &body).await?;
        // the write succeeded; an unreadable echo must not look like a failed post
        write::posted_comment(&v, c).ok_or(ProviderError::PostedUnreadable)
    }

    async fn set_resolved(
        &self,
        _repo: &RepoRef,
        _id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), ProviderError> {
        let node = write::resolve_target(thread)?;
        let query = if resolved {
            graphql::RESOLVE_THREAD
        } else {
            graphql::UNRESOLVE_THREAD
        };
        self.client
            .graphql_mutation(query, json!({ "threadId": node }))
            .await?;
        Ok(())
    }

    /// The trait carries no reviewed revision here: `commit_id` is the head `GET /pulls/{n}`
    /// reports right before the POST (the app has just checked it equals the one on screen).
    /// My own PR is refused before the POST (GitHub does not let an author approve or request
    /// changes on their own PR).
    async fn set_review(
        &self,
        repo: &RepoRef,
        id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), ProviderError> {
        let (event, text) = write::review_event(a, body)?;
        let pr = self.pr(repo, id).await?;
        let me = self.me_cached().await?;
        if !me.id.is_empty() && me.id == pr.author.id {
            return Err(ProviderError::Invalid(
                "you cannot approve or request changes on your own pull request".into(),
            ));
        }
        if pr.source_commit.trim().is_empty() {
            return Err(ProviderError::Decode("pull request without head".into()));
        }
        let path = format!("{}/pulls/{id}/reviews", Self::repo_path(repo));
        let body = write::review_body(event, text.as_deref(), &pr.source_commit);
        self.client.write(Method::POST, &path, &body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_gate_allows_one_per_minute() {
        let gate = MinInterval::new(SEARCH_EVERY);
        let t0 = Instant::now();
        assert!(gate.try_at(t0));
        assert!(!gate.try_at(t0));
        assert!(!gate.try_at(t0 + Duration::from_secs(59)));
        assert!(gate.try_at(t0 + Duration::from_secs(60)));
        assert!(!gate.try_at(t0 + Duration::from_secs(61)));
        assert!(gate.try_at(t0 + Duration::from_secs(125)));
    }
}
