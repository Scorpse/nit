use super::*;

/// `CommentPoster` backed by the provider of the repository's account; blocks the calling
/// worker thread.
pub(super) struct ProviderPoster {
    pub(super) reg: Arc<ProviderRegistry>,
    pub(super) rt: tokio::runtime::Handle,
}

impl ProviderPoster {
    /// The provider of `repo`'s account (blocking secret reads on first use: worker threads
    /// only).
    pub(super) fn provider(&self, repo: &RepoRef) -> Result<Arc<dyn Provider>, PostError> {
        self.reg
            .for_repo(repo)
            .map_err(|_| PostError::Network("not signed in".into()))
    }
}

/// Category text only: no URLs, bodies or credentials reach the UI.
pub(super) fn post_error(e: &ProviderError) -> PostError {
    match e {
        ProviderError::PostedUnreadable => PostError::PostedUnreadable,
        ProviderError::Unconfirmed => PostError::Unconfirmed,
        ProviderError::Network(_) => PostError::Network(e.user_message()),
        ProviderError::Invalid(_) => PostError::Invalid(e.user_message()),
        _ => PostError::Rejected(e.user_message()),
    }
}

impl CommentPoster for ProviderPoster {
    fn post(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        at: &PostRevision,
        c: &NewComment,
    ) -> Result<Comment, PostError> {
        self.rt
            .block_on(self.provider(repo)?.post_comment(repo, pr_id, at, c))
            .map_err(|e| post_error(&e))
    }

    fn resolve(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<(), PostError> {
        self.rt
            .block_on(
                self.provider(repo)?
                    .set_resolved(repo, pr_id, thread, resolved),
            )
            .map_err(|e| post_error(&e))
    }

    fn current_head(&self, repo: &RepoRef, pr_id: u64) -> Result<String, PostError> {
        self.rt
            .block_on(self.provider(repo)?.pr(repo, pr_id))
            .map(|pr| pr.source_commit)
            .map_err(|e| post_error(&e))
    }

    fn anchor_model(&self, repo: &RepoRef) -> AnchorModel {
        // without a provider nothing is posted anyway; the strictest check is the default
        self.provider(repo)
            .map_or(AnchorModel::DiffRefs, |p| p.capabilities().inline_anchor)
    }

    fn current_revision(&self, repo: &RepoRef, pr_id: u64) -> Result<ServerRevision, PostError> {
        self.rt
            .block_on(self.provider(repo)?.revision(repo, pr_id))
            .map_err(|e| post_error(&e))
    }

    fn current_pr(&self, repo: &RepoRef, pr_id: u64) -> Result<PullRequest, PostError> {
        self.rt
            .block_on(self.provider(repo)?.pr(repo, pr_id))
            .map_err(|e| post_error(&e))
    }

    fn set_review(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        a: ReviewAction,
        body: Option<&str>,
    ) -> Result<(), PostError> {
        self.rt
            .block_on(set_review_checked(
                self.provider(repo)?.as_ref(),
                repo,
                pr_id,
                a,
                body,
            ))
            .map_err(|e| post_error(&e))
    }
}
