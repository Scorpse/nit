use super::*;

/// What one review open of `repo` needs from the sign-in of the account it belongs to
/// (blocking secret reads: call it off the UI thread).
pub(super) fn review_deps(
    secrets: &dyn SecretStore,
    reg: &ProviderRegistry,
    handle: &tokio::runtime::Handle,
    repo: &RepoRef,
) -> Arc<ReviewDeps> {
    let owner = reg.owner(repo);
    let auth = reg.account(&owner).and_then(|a| git_auth(secrets, &a));
    let provider = reg.get(&owner).ok();
    let (p2, p3, p4) = (provider.clone(), provider.clone(), provider.clone());
    let (h1, h2, h3, h4) = (
        handle.clone(),
        handle.clone(),
        handle.clone(),
        handle.clone(),
    );
    let probe: HeadProbe = Arc::new(move |repo, id| {
        let p = provider
            .as_ref()
            .ok_or_else(|| "not signed in".to_string())?;
        h1.block_on(p.pr(repo, id))
            .map(|pr| pr.source_commit)
            .map_err(|e| e.user_message())
    });
    let comments: CommentFetcher = Arc::new(move |repo, id| {
        let p = p2.as_ref().ok_or_else(|| "not signed in".to_string())?;
        h2.block_on(p.comments(repo, id))
            .map_err(|e| e.user_message())
    });
    let last_review: LastReviewFetcher = Arc::new(move |repo, id| {
        let p = p3.as_ref().ok_or_else(|| "not signed in".to_string())?;
        h3.block_on(p.my_last_review(repo, id))
            .map_err(|e| e.user_message())
    });
    let create_task: TaskCreator = Arc::new(move |repo, id, content| {
        let p = p4.as_ref().ok_or_else(|| "not signed in".to_string())?;
        h4.block_on(p.create_task(repo, id, content))
            .map_err(|e| e.user_message())
    });
    Arc::new(ReviewDeps {
        auth,
        probe,
        comments,
        last_review,
        create_task,
    })
}

/// Git credentials of `account`, scoped to its git host (spec 7.2). Bitbucket needs both
/// stored values (a half sign-in is no sign-in); git authenticates with the token alone.
pub(super) fn git_auth(secrets: &dyn SecretStore, account: &Account) -> Option<GitAuth> {
    let token = secrets.get(&token_key(&account.id)).ok().flatten()?;
    match account.forge {
        Forge::BitbucketCloud => {
            secrets.get(&email_key(&account.id)).ok().flatten()?;
            (account.git_host == prcore::model::BITBUCKET_HOST).then(|| GitAuth::bitbucket(&token))
        }
        // user name per GitHub's docs; the header goes to the account's git host only
        //
        Forge::GitHub => forge_git_auth(
            gitlocal::GITHUB_GIT_USER,
            &token,
            &account.web_base,
            &account.git_host,
        ),
        // user oauth2 per GitLab's docs; the header goes to the account's host only (a
        // relative URL root's prefix is dropped from the scope; `multi-forge-verification.md`,
        // P3 git transport)
        Forge::GitLab => forge_git_auth(
            gitlocal::GITLAB_GIT_USER,
            &token,
            &account.web_base,
            &account.git_host,
        ),
    }
}

/// Git credentials of a GitHub or GitLab account for `git_host`: over plain http only when
/// its web address is http on that same host and the host is a private address
/// (`GitAuth::cleartext`, None otherwise), else https.
pub(crate) fn forge_git_auth(
    user: &str,
    token: &str,
    web_base: &str,
    git_host: &str,
) -> Option<GitAuth> {
    let web = web_base.to_ascii_lowercase();
    let Some(rest) = web.strip_prefix("http://") else {
        return Some(GitAuth::new(user, token, git_host));
    };
    let web_host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let git = git_host.split('/').next().unwrap_or(git_host);
    if web_host.eq_ignore_ascii_case(git) {
        GitAuth::cleartext(user, token, git_host)
    } else {
        Some(GitAuth::new(user, token, git_host))
    }
}

/// One provider per account, built on first use from that account's stored credentials and
/// dropped when they change (sign-in, sign-out, removal). Every provider uses the shared
/// connection pool.
pub(super) struct ProviderRegistry {
    pub(super) secrets: Arc<dyn SecretStore>,
    pub(super) store: Arc<Mutex<Store>>,
    pub(super) by_account: Mutex<HashMap<AccountId, Arc<dyn Provider>>>,
}

impl ProviderRegistry {
    pub(super) fn new(secrets: Arc<dyn SecretStore>, store: Arc<Mutex<Store>>) -> Self {
        ProviderRegistry {
            secrets,
            store,
            by_account: Mutex::new(HashMap::new()),
        }
    }

    /// The account row; the default sign-in also without one (saved before the row was).
    pub(super) fn account(&self, id: &AccountId) -> Option<Account> {
        let found = lock_ok(&self.store)
            .accounts()
            .ok()?
            .into_iter()
            .find(|a| a.id == *id);
        match found {
            Some(a) => Some(a),
            None if *id == AccountId::default_bitbucket() => {
                Some(Account::default_bitbucket(String::new(), String::new()))
            }
            None => None,
        }
    }

    /// The account `repo` is reached through (the default one when no account lists it).
    pub(super) fn owner(&self, repo: &RepoRef) -> AccountId {
        lock_ok(&self.store)
            .repo_account(repo)
            .ok()
            .flatten()
            .unwrap_or_else(AccountId::default_bitbucket)
    }

    /// Blocking secret reads on the first call per account: keep it off the UI thread.
    pub(super) fn get(&self, id: &AccountId) -> Result<Arc<dyn Provider>, String> {
        if let Some(p) = lock_ok(&self.by_account).get(id) {
            return Ok(p.clone());
        }
        let account = self.account(id).ok_or("No such account")?;
        let p = build_provider(&account, self.secrets.as_ref())?;
        lock_ok(&self.by_account).insert(id.clone(), p.clone());
        Ok(p)
    }

    /// The provider of the account `repo` is reached through.
    pub(super) fn for_repo(&self, repo: &RepoRef) -> Result<Arc<dyn Provider>, String> {
        self.get(&self.owner(repo))
    }

    pub(super) fn clear(&self) {
        lock_ok(&self.by_account).clear();
    }
}

/// Builds `account`'s provider; the token exists only inside this function and the client.
pub(super) fn build_provider(
    account: &Account,
    secrets: &dyn SecretStore,
) -> Result<Arc<dyn Provider>, String> {
    // https, or plain http on a private address only
    if !prcore::origin::transport_allowed(&account.api_base) {
        return Err("The account's API address is not https".into());
    }
    match account.forge {
        Forge::BitbucketCloud => {
            let email = secrets.get(&email_key(&account.id)).ok().flatten();
            let token = secrets.get(&token_key(&account.id)).ok().flatten();
            match (email, token) {
                (Some(email), Some(token)) => Ok(Arc::new(BitbucketProvider::new(BbClient::new(
                    &account.api_base,
                    &email,
                    &token,
                )))),
                _ => Err("No stored credentials".to_string()),
            }
        }
        Forge::GitHub => {
            // GraphQL must be https on the API's own origin (the client refuses any other)
            let graphql = account
                .graphql_base
                .clone()
                .unwrap_or_else(|| format!("{}/graphql", account.api_base));
            if !prcore::origin::transport_allowed(&graphql) {
                return Err("The account's GraphQL address is not https".into());
            }
            match secrets.get(&token_key(&account.id)).ok().flatten() {
                Some(token) => Ok(Arc::new(GitHubProvider::new(
                    &account.api_base,
                    &graphql,
                    &account.git_host,
                    &token,
                ))),
                None => Err("No stored credentials".to_string()),
            }
        }
        Forge::GitLab => match secrets.get(&token_key(&account.id)).ok().flatten() {
            Some(token) => Ok(Arc::new(GitLabProvider::new(
                &account.api_base,
                &account.git_host,
                &token,
            ))),
            None => Err("No stored credentials".to_string()),
        },
    }
}
