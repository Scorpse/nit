//! GitLab (gitlab.com, self-managed incl. relative URL roots) implementation of
//! [`Provider`]. Writes (spec 12, P4) are in `write`; inline posting stays gated by
//! `INLINE_ENABLED_GITLAB` in the app.

mod bases;
mod client;
mod comments;
mod inbox;
mod mark;
pub mod position;
mod write;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use prcore::comments::{AnchorModel, Comment, NewComment, PrTask, ReviewAction, ThreadId};
use prcore::history::ReviewMark;
use prcore::model::{DiffStat, Forge, PrState, PullRequest, RepoRef, UserRef};
use reqwest::Method;
use serde_json::{json, Value};

pub use bases::{bases_for, GitLabBases, GITLAB_COM};
pub use client::{GlClient, DEFAULT_PAUSE, MAX_INLINE_WAIT, MIN_WRITE_SPACING};
pub use inbox::{parse_changes_count, CAPPED_FILES};
pub use write::{INLINE_ENABLED_GITLAB, QUICK_ACTION, REQUEST_CHANGES};

use crate::provider::{
    Capabilities, CommentList, Discovered, PostRevision, PrList, Provider, ProviderError,
    ServerRevision, SuggestionSyntax,
};

/// What GitLab supports (spec 4.3): approve and unapprove, request changes (reviewer state;
/// there is no reviewer-side clear), threads on general comments.
pub const CAPABILITIES: Capabilities = Capabilities {
    request_changes: true,
    clear_approval: true,
    clear_request_changes: false,
    resolve_threads: true,
    reply_to_general_comments: true,
    file_comments: true,
    multi_line_comments: true,
    suggestions: Some(SuggestionSyntax::GitLabRange),
    tasks: false,
    inline_anchor: AnchorModel::DiffRefs,
    batch_review: false,
    read_only: false,
};

/// Every this many open-MR syncs of a repository the whole list is read again (without
/// `updated_after`), which drops MRs closed or merged since (spec 8.2).
pub const FULL_LIST_EVERY: u32 = 10;

/// One repository's open MRs as last synced.
#[derive(Clone, Default)]
struct OpenState {
    /// The newest `updated_at` seen, exactly as GitLab wrote it (the next `updated_after`).
    cursor: Option<String>,
    prs: BTreeMap<u64, PullRequest>,
    syncs: u32,
}

/// One GitLab account.
pub struct GitLabProvider {
    client: GlClient,
    /// The account's host with any path prefix ("gitlab.com", "git.corp.example/gitlab").
    host: String,
    me: Mutex<Option<UserRef>>,
    open: Mutex<HashMap<String, OpenState>>,
    /// Numeric project ids seen in discovery, by repository key (used instead of the path).
    ids: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for GitLabProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabProvider")
            .field("client", &self.client)
            .field("host", &self.host)
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Percent-encodes one path segment or query value: a full project path becomes
/// `group%2Fsub%2Fproject` (each `/` once, spec 8.2).
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

impl GitLabProvider {
    /// `api_base` per spec 5.1 (`https://gitlab.com/api/v4`, `https://host/prefix/api/v4`);
    /// `host` is the account's host with any prefix (the `RepoRef` host).
    pub fn new(api_base: &str, host: &str, token: &str) -> GitLabProvider {
        GitLabProvider {
            client: GlClient::new(api_base, token),
            host: host.trim_end_matches('/').to_ascii_lowercase(),
            me: Mutex::new(None),
            open: Mutex::new(HashMap::new()),
            ids: Mutex::new(HashMap::new()),
        }
    }

    /// The HTTP client (rate-limit state, request counter).
    pub fn client(&self) -> &GlClient {
        &self.client
    }

    /// `/projects/<id>`: the numeric id once discovery saw it, else the encoded full path.
    fn project_path(&self, repo: &RepoRef) -> String {
        match lock(&self.ids).get(&repo.key()) {
            Some(id) => format!("/projects/{id}"),
            None => format!("/projects/{}", enc(&repo.full_name())),
        }
    }

    fn mrs_path(&self, repo: &RepoRef) -> String {
        format!("{}/merge_requests", self.project_path(repo))
    }

    async fn me_cached(&self) -> Result<UserRef, ProviderError> {
        if let Some(me) = lock(&self.me).clone() {
            return Ok(me);
        }
        let me = self.me().await?;
        *lock(&self.me) = Some(me.clone());
        Ok(me)
    }

    /// The token's own details (`GET /personal_access_tokens/self`): scopes, expiry,
    /// active. For the sign-in check only.
    pub async fn token_info(&self) -> Result<TokenInfo, ProviderError> {
        let v = self.client.get_json("/personal_access_tokens/self").await?;
        Ok(TokenInfo {
            scopes: v["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect(),
            expires_at: v["expires_at"]
                .as_str()
                .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
            active: v["active"].as_bool().unwrap_or(true),
        })
    }

    /// Reviewers and approvals of one MR, applied to it. A refused or missing answer leaves
    /// the list's reviewers (all pending) and no "needs me" from that part.
    async fn review_state(&self, repo: &RepoRef, pr: &mut PullRequest, me: &UserRef) {
        let base = format!("{}/{}", self.mrs_path(repo), pr.id);
        let (rp, ap) = (format!("{base}/reviewers"), format!("{base}/approvals"));
        let (reviewers, approvals) =
            tokio::join!(self.client.get_json(&rp), self.client.get_json(&ap));
        let reviewers = reviewers.ok();
        let list = reviewers
            .as_ref()
            .and_then(Value::as_array)
            .map(Vec::as_slice);
        inbox::apply_review_state(pr, list, approvals.ok().as_ref(), Some(me));
    }

    /// Open MRs: `state=opened&updated_after=<cursor>`, merged into the last result; every
    /// `FULL_LIST_EVERY`th sync (and the first) reads the whole list again. Reviewers and
    /// approvals are asked only for MRs whose `updated_at` moved.
    async fn open_prs(&self, repo: &RepoRef) -> Result<PrList, ProviderError> {
        let key = repo.key();
        let mut state = lock(&self.open).get(&key).cloned().unwrap_or_default();
        let full = state.syncs % FULL_LIST_EVERY == 0 || state.cursor.is_none();
        let mut path = format!(
            "{}?state=opened&scope=all&order_by=updated_at&sort=desc&per_page=100",
            self.mrs_path(repo)
        );
        if let (false, Some(c)) = (full, &state.cursor) {
            path.push_str(&format!("&updated_after={}", enc(c)));
        }
        let items = self.client.get_paged(&path).await?;
        let me = self.me_cached().await?;
        let mut skipped = Vec::new();
        let mut seen = BTreeMap::new();
        for v in &items {
            match inbox::mr(v, repo) {
                Ok(mut pr) if pr.state == PrState::Open => {
                    let changed = state
                        .prs
                        .get(&pr.id)
                        .is_none_or(|old| old.updated_on != pr.updated_on);
                    if changed {
                        self.review_state(repo, &mut pr, &me).await;
                    } else if let Some(old) = state.prs.get(&pr.id) {
                        pr.reviewers = old.reviewers.clone();
                        pr.review_requested_from_me = old.review_requested_from_me;
                    }
                    if let Some(t) = inbox::str_at(v, "updated_at") {
                        if state.cursor.as_ref().is_none_or(|c| newer(t, c)) {
                            state.cursor = Some(t.to_string());
                        }
                    }
                    seen.insert(pr.id, pr);
                }
                Ok(_) => {}
                Err(s) => skipped.push(s),
            }
        }
        if full {
            state.prs = seen;
        } else {
            state.prs.extend(seen);
        }
        state.syncs = state.syncs.wrapping_add(1);
        let mut prs: Vec<PullRequest> = state.prs.values().cloned().collect();
        prs.sort_by(|a, b| b.updated_on.cmp(&a.updated_on).then(a.id.cmp(&b.id)));
        lock(&self.open).insert(key, state);
        Ok(PrList { prs, skipped })
    }

    /// One page of MRs in `state` (GitLab word), keeping those that decode to `keep`.
    async fn list(
        &self,
        path: &str,
        repo: &RepoRef,
        keep: PrState,
    ) -> Result<PrList, ProviderError> {
        let v = self.client.get_json(path).await?;
        let Some(items) = v.as_array() else {
            return Err(ProviderError::Decode("expected a list".into()));
        };
        let mut out = PrList::default();
        for item in items {
            match inbox::mr(item, repo) {
                Ok(pr) if pr.state == keep => out.prs.push(pr),
                Ok(_) => {}
                Err(s) => out.skipped.push(s),
            }
        }
        Ok(out)
    }

    async fn detail(&self, repo: &RepoRef, id: u64) -> Result<Value, ProviderError> {
        self.client
            .get_json(&format!("{}/{id}", self.mrs_path(repo)))
            .await
    }
}

/// `a` is later than `b` (both GitLab timestamps; unparsable ones compare as text).
fn newer(a: &str, b: &str) -> bool {
    let p = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (p(a), p(b)) {
        (Some(x), Some(y)) => x > y,
        _ => a > b,
    }
}

/// What `GET /personal_access_tokens/self` says about the token (no secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::NaiveDate>,
    pub active: bool,
}

#[async_trait]
impl Provider for GitLabProvider {
    fn forge(&self) -> Forge {
        Forge::GitLab
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn paused_until(&self) -> Option<std::time::SystemTime> {
        self.client.paused_until()
    }

    async fn me(&self) -> Result<UserRef, ProviderError> {
        let v = self.client.get_json("/user").await?;
        if v["id"].as_u64().is_none() || v["username"].as_str().is_none() {
            return Err(ProviderError::Decode("user without id or username".into()));
        }
        Ok(inbox::user(Some(&v)).0)
    }

    /// `GET /projects?membership=true&archived=false&simple=true`, every page. One listing:
    /// an error is `Err`, never a failed namespace.
    async fn discover(&self) -> Result<Discovered, ProviderError> {
        let items = self
            .client
            .get_paged("/projects?membership=true&archived=false&simple=true&per_page=100")
            .await?;
        let mut out = Discovered::default();
        let mut ids = lock(&self.ids);
        for v in &items {
            let Some(info) = inbox::project(v, &self.host) else {
                continue;
            };
            if let Some(id) = v["id"].as_u64() {
                ids.insert(info.repo.key(), id);
            }
            if !out.namespaces.contains(&info.repo.namespace) {
                out.namespaces.push(info.repo.namespace.clone());
            }
            out.repos.push(info);
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
        let word = match state {
            PrState::Open => return self.open_prs(repo).await,
            PrState::Superseded => return Ok(PrList::default()),
            _ if max == 0 => return Ok(PrList::default()),
            PrState::Merged => "merged",
            PrState::Declined => "closed",
        };
        let path = format!(
            "{}?state={word}&scope=all&order_by=updated_at&sort=desc&per_page={}",
            self.mrs_path(repo),
            max.min(100)
        );
        self.list(&path, repo, state).await
    }

    async fn merged_by_source_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<PrList, ProviderError> {
        let path = format!(
            "{}?state=merged&scope=all&source_branch={}&order_by=updated_at&sort=desc&per_page=100",
            self.mrs_path(repo),
            enc(branch)
        );
        self.list(&path, repo, PrState::Merged).await
    }

    /// `changes_count` of the single MR (the list does not carry it). Not computed yet:
    /// `Decode`, so no size is stored and the next sync asks again.
    async fn diffstat(&self, repo: &RepoRef, id: u64) -> Result<DiffStat, ProviderError> {
        let v = self.detail(repo, id).await?;
        inbox::stat(&v).ok_or_else(|| ProviderError::Decode("size not computed yet".into()))
    }

    async fn pr(&self, repo: &RepoRef, id: u64) -> Result<PullRequest, ProviderError> {
        let v = self.detail(repo, id).await?;
        inbox::mr(&v, repo).map_err(|s| ProviderError::Decode(format!("field {}", s.field)))
    }

    /// Head from the MR; base and start from the latest diff version, else `diff_refs`.
    /// Both empty (just created, computed asynchronously): no base, which blocks inline.
    async fn revision(&self, repo: &RepoRef, id: u64) -> Result<ServerRevision, ProviderError> {
        let v = self.detail(repo, id).await?;
        let pr =
            inbox::mr(&v, repo).map_err(|s| ProviderError::Decode(format!("field {}", s.field)))?;
        let versions = self
            .client
            .get_json(&format!("{}/{id}/versions", self.mrs_path(repo)))
            .await
            .ok();
        let latest = versions
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|l| l.iter().max_by_key(|x| x["id"].as_u64().unwrap_or(0)));
        let sha = |obj: Option<&Value>, k: &str| {
            obj.and_then(|o| inbox::str_at(o, k))
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let refs = v.get("diff_refs").filter(|d| d.is_object());
        Ok(ServerRevision {
            head: pr.source_commit,
            base: sha(latest, "base_commit_sha").or_else(|| sha(refs, "base_sha")),
            start: sha(latest, "start_commit_sha").or_else(|| sha(refs, "start_sha")),
            dest_branch: pr.dest_branch,
        })
    }

    /// Every discussion (`/discussions`, paged), placed against the MR's current revision
    /// (a diff note made on another head or base is outdated, spec 7.4).
    async fn comments(&self, repo: &RepoRef, id: u64) -> Result<CommentList, ProviderError> {
        let path = format!("{}/{id}/discussions?per_page=100", self.mrs_path(repo));
        let (current, discussions) =
            tokio::join!(self.revision(repo, id), self.client.get_paged(&path));
        Ok(comments::decode(&discussions?, &current?))
    }

    /// The newest note of mine (`/notes`, newest first, one page of 100).
    async fn my_last_review(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> Result<Option<ReviewMark>, ProviderError> {
        let path = format!(
            "{}/{id}/notes?order_by=created_at&sort=desc&per_page=100",
            self.mrs_path(repo)
        );
        let (me, notes) = tokio::join!(self.me_cached(), self.client.get_json(&path));
        Ok(mark::mark_of(&notes?, &me?.id))
    }

    async fn tasks(&self, _repo: &RepoRef, _id: u64) -> Result<Vec<PrTask>, ProviderError> {
        Err(ProviderError::Invalid("GitLab has no MR tasks".into()))
    }

    /// MR comment as a note; file and line comments as a discussion whose `position` uses the
    /// diff version of the reviewed head (`at.head`), never "the latest" (the app checks the
    /// head, the base and, for inline, `INLINE_ENABLED_GITLAB` before calling).
    async fn post_comment(
        &self,
        repo: &RepoRef,
        id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, ProviderError> {
        let text = match c {
            NewComment::Pr { text }
            | NewComment::File { text, .. }
            | NewComment::Inline { text, .. }
            | NewComment::Reply { text, .. } => text,
        };
        write::quick_action_guard(text)?;
        let mr = format!("{}/{id}", self.mrs_path(repo));
        let mut current = ServerRevision {
            head: at.head.clone(),
            ..ServerRevision::default()
        };
        let (path, body) = match c {
            NewComment::Pr { text } => (format!("{mr}/notes"), json!({ "body": text })),
            NewComment::Reply { thread, text } => (
                format!("{mr}/discussions/{}/notes", write::discussion_id(thread)?),
                json!({ "body": text }),
            ),
            NewComment::File { text, .. } | NewComment::Inline { text, .. } => {
                if let NewComment::Inline { line, start, .. } = c {
                    if *line == 0 || start.is_some_and(|(_, l)| l == 0) {
                        return Err(ProviderError::Invalid(
                            "inline line numbers start at 1".into(),
                        ));
                    }
                }
                let versions = self.client.get_json(&format!("{mr}/versions")).await?;
                let v = write::version_for(&versions, &at.head)?;
                let vid = v["id"]
                    .as_u64()
                    .ok_or_else(|| ProviderError::Decode("version without id".into()))?;
                let version = self
                    .client
                    .get_json(&format!("{mr}/versions/{vid}"))
                    .await?;
                let position = write::position_for(&version, at, c)?;
                current.base = position["base_sha"].as_str().map(str::to_string);
                (
                    format!("{mr}/discussions"),
                    json!({ "body": text, "position": position }),
                )
            }
        };
        let v = self.client.write(Method::POST, &path, Some(&body)).await?;
        // the write succeeded; an unreadable echo must not look like a failed post
        write::posted_comment(&v, c, &current).ok_or(ProviderError::PostedUnreadable)
    }

    async fn set_resolved(
        &self,
        repo: &RepoRef,
        id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), ProviderError> {
        let d = write::discussion_id(thread)?;
        let path = format!(
            "{}/{id}/discussions/{d}?resolved={resolved}",
            self.mrs_path(repo)
        );
        self.client.write(Method::PUT, &path, None).await?;
        Ok(())
    }

    /// Approve sends the head GitLab reports right before the POST as `sha` (the app has just
    /// checked it equals the one on screen; a push in between is a 409). Request changes is
    /// the GraphQL mutation. `body` is not used (GitLab takes no text with these).
    async fn set_review(
        &self,
        repo: &RepoRef,
        id: u64,
        a: ReviewAction,
        _body: Option<&str>,
    ) -> Result<(), ProviderError> {
        if a == ReviewAction::ClearRequestChanges {
            return Err(ProviderError::Invalid(
                "this action is not available on this forge".into(),
            ));
        }
        let mr = format!("{}/{id}", self.mrs_path(repo));
        let head = match a {
            ReviewAction::Approve => self.pr(repo, id).await?.source_commit,
            _ => String::new(),
        };
        let result = match write::review_request(a, &head)? {
            Some((suffix, body)) => self
                .client
                .write(Method::POST, &format!("{mr}/{suffix}"), body.as_ref())
                .await
                .map(|_| ()),
            None => self
                .client
                .graphql_mutation(
                    "mergeRequestRequestChanges",
                    write::REQUEST_CHANGES,
                    json!({ "projectPath": repo.full_name(), "iid": id.to_string() }),
                )
                .await
                .map(|_| ()),
        };
        result.map_err(|e| write::review_error(a, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_encodes_every_slash_once() {
        assert_eq!(
            enc("my-group/sub/my-project"),
            "my-group%2Fsub%2Fmy-project"
        );
        assert_eq!(enc("feature/a b"), "feature%2Fa%20b");
    }

    #[test]
    fn newer_compares_times_not_text() {
        assert!(newer(
            "2026-11-11T09:00:00.000Z",
            "2026-11-10T10:00:00.000Z"
        ));
        assert!(!newer("2026-11-10T10:00:00Z", "2026-11-10T10:00:00.000Z"));
    }

    #[test]
    fn writes_are_enabled() {
        const _: () = assert!(!CAPABILITIES.read_only && INLINE_ENABLED_GITLAB);
        assert!(CAPABILITIES.allows(ReviewAction::Approve));
        assert!(CAPABILITIES.allows(ReviewAction::ClearApproval));
        assert!(CAPABILITIES.allows(ReviewAction::RequestChanges));
        assert!(!CAPABILITIES.allows(ReviewAction::ClearRequestChanges));
    }
}
