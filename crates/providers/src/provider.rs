//! Provider abstraction and its error type.

use async_trait::async_trait;
pub use prcore::comments::{AnchorModel, PostRevision, ServerRevision};
use prcore::comments::{Comment, NewComment, PrTask, ReviewAction, ThreadId};
use prcore::history::ReviewMark;
use prcore::model::{DiffStat, Forge, PrState, PullRequest, RepoInfo, RepoRef, UserRef};
use thiserror::Error;

use crate::bb_client::CERT_UNTRUSTED;

/// Errors returned by provider clients. `Display` never contains credentials.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProviderError {
    #[error("authentication failed")]
    Auth,
    #[error("access forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("rate limited")]
    RateLimited,
    #[error("unexpected HTTP status {0}")]
    Http(u16),
    #[error("network error: {0}")]
    Network(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("posted, but the response could not be read")]
    PostedUnreadable,
    /// A comment POST was sent but no answer arrived (timeout, reset, 5xx): it may or may
    /// not have been created.
    #[error("sent, but whether it was accepted is unknown")]
    Unconfirmed,
    /// A write was refused (400/403/404/409) with Bitbucket's own short, single-line,
    /// sanitized error message.
    #[error("rejected with status {status}: {message}")]
    Rejected { status: u16, message: String },
}

const CERT_HINT: &str = "the server's certificate was not trusted. If you are on a company network, ask IT to install the company root certificate in Windows, or set HTTPS_PROXY";

impl ProviderError {
    /// Short failure category (sync repo errors); never includes URLs, bodies or credentials.
    /// The UI matches on "access denied": keep the strings.
    pub fn category(&self) -> String {
        match self {
            ProviderError::Auth => "authentication failed".into(),
            ProviderError::Forbidden => "access denied".into(),
            ProviderError::NotFound => "not found".into(),
            ProviderError::RateLimited => "rate limited".into(),
            ProviderError::Http(c) => format!("http {c}"),
            ProviderError::Network(_) => "network error".into(),
            ProviderError::Decode(_) => "decode error".into(),
            ProviderError::Invalid(_) => "invalid request".into(),
            ProviderError::PostedUnreadable => "posted, response unreadable".into(),
            ProviderError::Unconfirmed => "outcome unknown".into(),
            ProviderError::Rejected { status, .. } => format!("http {status}"),
        }
    }

    /// The category as shown for the user's own actions: a hint for an untrusted certificate
    /// and, on a refused write, the server's own short message (one line, sanitized by the
    /// client). Never includes URLs, bodies or credentials.
    pub fn user_message(&self) -> String {
        match self {
            ProviderError::Network(m) if m == CERT_UNTRUSTED => CERT_HINT.into(),
            ProviderError::Unconfirmed => "sent, outcome unknown".into(),
            ProviderError::Rejected { status, message } => {
                let what = match status {
                    403 => "access denied".to_string(),
                    404 => "not found".to_string(),
                    s => format!("http {s}"),
                };
                format!("{what}: {message}")
            }
            e => e.category(),
        }
    }
}

/// A list entry that could not be decoded. Holds the PR id (when readable) and the path of the
/// offending field, never field values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedPr {
    pub id: Option<u64>,
    pub field: String,
}

/// One PR list: the decoded PRs plus the entries that were skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrList {
    pub prs: Vec<PullRequest>,
    pub skipped: Vec<SkippedPr>,
}

/// A comment entry that could not be decoded: its id (when readable) and the path of the
/// offending field, never values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedComment {
    pub id: Option<u64>,
    pub field: String,
}

/// All comments of a PR plus the entries that were skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentList {
    pub comments: Vec<Comment>,
    pub skipped: Vec<SkippedComment>,
    /// The PR's tasks (read-only); `None` when they could not be fetched.
    pub tasks: Option<Vec<PrTask>>,
}

/// What one discovery listed (spec 4.3).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub repos: Vec<RepoInfo>,
    /// Every namespace found (workspace, owner, group), including the failed ones.
    pub namespaces: Vec<String>,
    /// Namespaces whose repositories could not be listed, with the error.
    pub failed: Vec<(String, ProviderError)>,
}

impl Discovered {
    /// The namespaces whose repositories were listed.
    pub fn listed_namespaces(&self) -> Vec<String> {
        self.namespaces
            .iter()
            .filter(|n| !self.failed.iter().any(|(f, _)| f == *n))
            .cloned()
            .collect()
    }
}

/// How a forge writes a suggested change in a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionSyntax {
    /// GitHub: a ```` ```suggestion ```` block replacing the commented lines.
    GitHubFence,
    /// GitLab: ```` ```suggestion:-N+M ```` with offsets around the commented line.
    GitLabRange,
}

/// What a forge supports (spec 4.3). The UI hides what is false, and an unsupported review
/// action is refused before any request (`refuse_unsupported`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub request_changes: bool,
    pub clear_approval: bool,
    pub clear_request_changes: bool,
    /// Per-thread `Comment::can_resolve` still gates the button.
    pub resolve_threads: bool,
    pub reply_to_general_comments: bool,
    pub file_comments: bool,
    pub multi_line_comments: bool,
    pub suggestions: Option<SuggestionSyntax>,
    pub tasks: bool,
    pub inline_anchor: AnchorModel,
    /// Pending review / draft notes; not used while drafts are posted one by one (D5).
    pub batch_review: bool,
    /// Nothing may be written for this forge in this version: every review action, comment,
    /// reply and resolve is refused and the UI hides the write buttons (no forge is
    /// read-only now; kept for a forge added read-first).
    pub read_only: bool,
}

impl Capabilities {
    /// Bitbucket Cloud, as the app has always used it.
    pub const BITBUCKET: Capabilities = Capabilities {
        request_changes: true,
        clear_approval: true,
        clear_request_changes: true,
        resolve_threads: true,
        reply_to_general_comments: true,
        file_comments: true,
        multi_line_comments: false,
        suggestions: None,
        tasks: true,
        inline_anchor: AnchorModel::CurrentDiff,
        batch_review: false,
        read_only: false,
    };

    /// Whether review action `a` exists on this forge (approve always does, unless the
    /// forge is read-only in this version).
    pub fn allows(&self, a: ReviewAction) -> bool {
        if self.read_only {
            return false;
        }
        match a {
            ReviewAction::Approve => true,
            ReviewAction::RequestChanges => self.request_changes,
            ReviewAction::ClearApproval => self.clear_approval,
            ReviewAction::ClearRequestChanges => self.clear_request_changes,
        }
    }
}

/// What a forge's provider supports, without building one (the UI decides which buttons
/// exist from the PR's forge).
pub fn capabilities_for(forge: prcore::model::Forge) -> Capabilities {
    use prcore::model::Forge;
    match forge {
        Forge::BitbucketCloud => Capabilities::BITBUCKET,
        Forge::GitHub => crate::github::CAPABILITIES,
        Forge::GitLab => crate::gitlab::CAPABILITIES,
    }
}

/// `Invalid` for an action the forge does not support; called before any request is built.
pub fn refuse_unsupported(caps: &Capabilities, a: ReviewAction) -> Result<(), ProviderError> {
    if caps.allows(a) {
        Ok(())
    } else {
        Err(ProviderError::Invalid(
            "this action is not available on this forge".into(),
        ))
    }
}

/// `Provider::set_review` behind the capability check: an unsupported action never reaches
/// the provider (no request is made).
pub async fn set_review_checked(
    p: &dyn Provider,
    repo: &RepoRef,
    id: u64,
    a: ReviewAction,
    body: Option<&str>,
) -> Result<(), ProviderError> {
    refuse_unsupported(&p.capabilities(), a)?;
    p.set_review(repo, id, a, body).await
}

/// A code-hosting provider (Bitbucket Cloud first).
#[async_trait]
pub trait Provider: Send + Sync {
    fn forge(&self) -> Forge;
    fn capabilities(&self) -> Capabilities;
    /// Until when this account's client refuses to send because the forge's rate limit was
    /// reached (shown per account in Settings and the inbox). None when not paused, and for
    /// a forge whose client does not pause.
    fn paused_until(&self) -> Option<std::time::SystemTime> {
        None
    }
    async fn me(&self) -> Result<UserRef, ProviderError>;
    /// The repository of every open PR that requests my review anywhere the account can see,
    /// one entry per PR, for a hint only (decision D4: nothing is ever selected from it).
    /// `Ok(None)`: the forge offers no such search, or it was asked too recently (the caller
    /// keeps its last answer).
    async fn review_requests_elsewhere(&self) -> Result<Option<Vec<RepoRef>>, ProviderError> {
        Ok(None)
    }
    /// Every repository the account can see, in one call (spec 4.3). Bitbucket walks its
    /// workspaces inside; a namespace whose repositories could not be listed is reported in
    /// `Discovered::failed`, not as `Err`. `Err` means nothing could be listed at all.
    async fn discover(&self) -> Result<Discovered, ProviderError>;
    async fn prs(
        &self,
        repo: &RepoRef,
        state: PrState,
        max: usize,
    ) -> Result<PrList, ProviderError>;
    /// Merged PRs whose source branch is `branch`, most recently updated first. One page only.
    async fn merged_by_source_branch(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<PrList, ProviderError>;
    async fn diffstat(&self, repo: &RepoRef, id: u64) -> Result<DiffStat, ProviderError>;

    /// The PR as it is right now (used to check `source_commit` before posting inline).
    async fn pr(&self, repo: &RepoRef, id: u64) -> Result<PullRequest, ProviderError>;
    /// What the server says the PR is right now, compared with the local fetch before an
    /// inline post.
    async fn revision(&self, repo: &RepoRef, id: u64) -> Result<ServerRevision, ProviderError>;
    async fn comments(&self, repo: &RepoRef, id: u64) -> Result<CommentList, ProviderError>;
    /// My last submitted review of the PR: when, and the commit it was made on where the forge
    /// says. `Ok(None)`: I have not reviewed it, or the forge does not tell.
    async fn my_last_review(
        &self,
        _repo: &RepoRef,
        _id: u64,
    ) -> Result<Option<ReviewMark>, ProviderError> {
        Ok(None)
    }
    /// The PR's tasks, read-only (unpublished drafts left out). Only called when
    /// `capabilities().tasks`; a forge without tasks answers `Invalid`.
    async fn tasks(&self, repo: &RepoRef, id: u64) -> Result<Vec<PrTask>, ProviderError>;
    /// Adds an open task with text `content` to the PR (not tied to a comment) and returns it
    /// as the server stored it. Only called when `capabilities().tasks`.
    async fn create_task(
        &self,
        _repo: &RepoRef,
        _id: u64,
        _content: &str,
    ) -> Result<PrTask, ProviderError> {
        Err(ProviderError::Invalid("this forge has no tasks".into()))
    }
    /// Posts `c` on the revision the user reviewed (`at`, the local fetch). A forge that pins
    /// comments to a commit sends exactly `at`, never "the latest"; Bitbucket anchors to the
    /// current diff and ignores it (guarded by `inline_allowed` before the call).
    async fn post_comment(
        &self,
        repo: &RepoRef,
        id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, ProviderError>;
    async fn set_resolved(
        &self,
        repo: &RepoRef,
        id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), ProviderError>;
    /// `body`: the review text where the forge takes one (GitHub requires it for
    /// request changes); ignored elsewhere. Call through `set_review_checked`.
    async fn set_review(
        &self,
        repo: &RepoRef,
        id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_has_no_detail() {
        let e = ProviderError::Network("https://x?token=secret".into());
        assert_eq!(e.category(), "network error");
        assert_eq!(e.user_message(), "network error");
        let cert = ProviderError::Network(CERT_UNTRUSTED.into());
        assert_eq!(cert.category(), "network error");
        let text = cert.user_message();
        assert!(text.contains("certificate") && text.contains("HTTPS_PROXY"));
    }

    #[test]
    fn capabilities_per_forge() {
        use prcore::model::Forge;
        assert_eq!(
            capabilities_for(Forge::BitbucketCloud),
            Capabilities::BITBUCKET
        );
        let gh = capabilities_for(Forge::GitHub);
        assert!(!gh.read_only && gh.allows(ReviewAction::Approve));
        assert!(!gh.allows(ReviewAction::ClearApproval));
        assert_eq!(gh.inline_anchor, AnchorModel::HeadCommit);
        let gl = capabilities_for(Forge::GitLab);
        assert!(!gl.read_only && gl.allows(ReviewAction::Approve));
        assert!(gl.allows(ReviewAction::ClearApproval) && gl.allows(ReviewAction::RequestChanges));
        assert!(!gl.allows(ReviewAction::ClearRequestChanges));
        assert_eq!(gl.inline_anchor, AnchorModel::DiffRefs);
        assert_eq!(gl.suggestions, Some(SuggestionSyntax::GitLabRange));
        assert!(!gl.tasks);
    }

    #[test]
    fn user_message_adds_the_server_message_on_a_refused_write() {
        let e = ProviderError::Rejected {
            status: 403,
            message: "Insufficient scope".into(),
        };
        assert_eq!(e.category(), "http 403");
        assert_eq!(e.user_message(), "access denied: Insufficient scope");
        assert_eq!(ProviderError::Forbidden.user_message(), "access denied");
        assert_eq!(ProviderError::Unconfirmed.category(), "outcome unknown");
        assert_eq!(
            ProviderError::Unconfirmed.user_message(),
            "sent, outcome unknown"
        );
    }
}
