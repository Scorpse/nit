use super::*;

// ---------------------------------------------------------------------------------------
// The checks

/// What the run needs besides the credentials.
pub struct Inputs<'a> {
    pub args: &'a CheckArgs,
    pub api_base: &'a str,
    pub git: &'a dyn GitSide,
    /// The proxy environment variable in use, by name (its value can hold a password).
    pub proxy_var: Option<&'static str>,
}

/// The signed-in user, as far as the check needs it (only the display name is shown).
pub(super) struct Me {
    pub(super) display_name: String,
    pub(super) username: Option<String>,
    pub(super) uuid: Option<String>,
}

pub(super) fn me_of(v: &Value) -> Me {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Me {
        display_name: s("display_name").unwrap_or_else(|| "(no display name)".into()),
        username: s("username"),
        uuid: s("uuid"),
    }
}

pub(super) fn host_of(base: &str) -> String {
    base.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(base)
        .to_string()
}

pub(super) fn pr_path(repo: &RepoRef, id: u64) -> String {
    format!(
        "/repositories/{}/{}/pullrequests/{id}",
        repo.namespace, repo.slug
    )
}

pub(super) fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Runs every check (read-only unless `--write-test`). Never panics on API answers.
pub async fn run(inp: &Inputs<'_>, creds: Result<Creds, String>) -> Report {
    let mut r = Report::default();
    let git_ok = check_git_version(&mut r, inp.git);
    let creds = match creds {
        Ok(c) => {
            r.source = Some(c.source);
            r.pass(1, format!("found in {}", c.source.label()));
            c
        }
        Err(why) => {
            r.fail(1, why, Some(HINT_CREDS.into()));
            for n in [2, 3, 4, 5, 6, 7, 9] {
                r.skip(n, "needs credentials (check 1)");
            }
            return r;
        }
    };
    let client = BbClient::new(inp.api_base, &creds.email, &creds.token);
    let provider = BitbucketProvider::new(BbClient::new(inp.api_base, &creds.email, &creds.token));

    // 2 and 9: one authenticated request answers both
    let me = client.get_json("/user").await;
    check_reach(
        &mut r,
        &me,
        inp.api_base,
        &host_of(inp.api_base),
        inp.proxy_var,
    );
    let me = match me {
        Ok(v) => {
            let me = me_of(&v);
            r.pass(2, format!("signed in as {}", me.display_name));
            Some(me)
        }
        Err(ProviderError::Network(_)) => {
            r.skip(2, "the API could not be reached (check 9)");
            None
        }
        Err(e) => {
            let detail = match &e {
                ProviderError::Auth => "Bitbucket refused the email or token (HTTP 401)".into(),
                ProviderError::Forbidden => {
                    "the token was accepted but may not read your user (HTTP 403)".into()
                }
                e => format!("GET /user failed: {}", e.user_message()),
            };
            r.fail(
                2,
                detail,
                api_hint(&e, "read access to your account (account: read)"),
            );
            None
        }
    };

    let found = if me.is_some() {
        let workspaces = check_workspaces(&mut r, &provider, inp.args).await;
        check_open_prs(&mut r, &provider, inp.args, &workspaces).await
    } else {
        r.skip(3, "needs a working REST sign-in (check 2)");
        r.skip(4, "needs a working REST sign-in (check 2)");
        Found::default()
    };

    // 5 and 6 need a PR: the given one, else the first open one found
    let pr = found.repo.clone().zip(inp.args.pr.or(found.first_pr));
    match &pr {
        Some((repo, id)) => {
            check_read_pr(&mut r, &provider, &client, repo, *id).await;
            check_approvals(&mut r, &provider, &client, repo, *id).await;
        }
        None => {
            let why = if me.is_none() {
                "needs a working REST sign-in (check 2)".to_string()
            } else {
                "no pull request to read: pass --repo <workspace>/<repo> --pr <id>".to_string()
            };
            r.skip(5, why.clone());
            r.skip(6, why);
        }
    }

    // 7: git, on the given repository or the one found above
    let git_repo = inp.args.repo.clone().or(found.repo).or(found.any_repo);
    match (git_repo, git_ok) {
        (Some(repo), true) => check_git_auth(&mut r, inp.git, &creds, me.as_ref(), &repo),
        (_, false) => r.skip(7, "needs a working git (check 8)"),
        (None, true) => r.skip(7, "no repository to try: pass --repo <workspace>/<repo>"),
    }

    if inp.args.write_test {
        match (&inp.args.repo, inp.args.pr, &me) {
            (Some(repo), Some(id), Some(_)) => {
                write_test(&mut r, &provider, &client, repo, id).await;
            }
            (Some(repo), Some(id), None) => {
                r.write_target = Some(format!("{} PR #{id}", repo.full_name()));
                r.step(
                    1,
                    "Post a PR comment",
                    Status::Skip,
                    "not attempted: the REST sign-in failed (check 2)".into(),
                    None,
                );
            }
            _ => {}
        }
    }
    r
}

/// What check 4 found for the checks after it.
#[derive(Default)]
pub(super) struct Found {
    /// The given repository, or the first one with open PRs.
    pub(super) repo: Option<RepoRef>,
    /// Its first open PR.
    pub(super) first_pr: Option<u64>,
    /// Any repository seen (for the git check when none has open PRs).
    pub(super) any_repo: Option<RepoRef>,
}

pub(super) fn check_reach(
    r: &mut Report,
    me: &Result<Value, ProviderError>,
    api_base: &str,
    host: &str,
    proxy_var: Option<&'static str>,
) {
    let plain = prcore::origin::is_private_http_url(api_base);
    let via = match proxy_var {
        Some(v) => format!(" (through the proxy in {v})"),
        None => " (no proxy variable set; the Windows system proxy applies if configured)".into(),
    };
    match me {
        Err(ProviderError::Network(m)) if m == CERT_UNTRUSTED => r.fail(
            9,
            format!("the certificate presented for {host} is not trusted{via}"),
            Some(cert_hint()),
        ),
        Err(ProviderError::Network(m)) => r.fail(
            9,
            format!("could not reach {host}: {m}{via}"),
            Some(
                "check the network connection; on a company network set HTTPS_PROXY to the \
                 proxy (proxies that need NTLM / Kerberos login are not supported)"
                    .into(),
            ),
        ),
        // any HTTP answer means TLS and the proxy worked
        _ if !plain => r.pass(9, format!("{host} answered over HTTPS{via}")),
        _ => r.pass(
            9,
            format!("{host} answered over plain HTTP (not encrypted, private address){via}"),
        ),
    }
}

pub(super) fn check_git_version(r: &mut Report, git: &dyn GitSide) -> bool {
    match git.version() {
        Ok(v) => {
            r.pass(8, v);
            true
        }
        Err(e) => {
            r.fail(
                8,
                e,
                Some(
                    "install Git for Windows 2.31 or newer (git-scm.com) and make sure `git` \
                     is on PATH, then run again"
                        .into(),
                ),
            );
            false
        }
    }
}

pub(super) const SCOPE_READ_REPO: &str = "Bitbucket read access to workspaces and repositories \
    (workspace: read, repository: read)";
pub(super) const SCOPE_READ_PR: &str = "Bitbucket read pull request access (pull request: read)";

/// Check 3; returns the visible workspaces.
pub(super) async fn check_workspaces(
    r: &mut Report,
    provider: &BitbucketProvider,
    args: &CheckArgs,
) -> Vec<String> {
    let workspaces = match provider.workspaces().await {
        Ok(w) => w,
        Err(e) => {
            r.fail(
                3,
                format!("could not list workspaces: {}", e.user_message()),
                api_hint(&e, SCOPE_READ_REPO),
            );
            return Vec::new();
        }
    };
    let ws = match (&args.repo, workspaces.first()) {
        (Some(repo), _) => repo.namespace.clone(),
        (None, Some(w)) => w.clone(),
        (None, None) => {
            r.fail(
                3,
                "the token sees no workspace",
                Some(format!(
                    "the account belongs to no workspace, or the token lacks {SCOPE_READ_REPO}"
                )),
            );
            return workspaces;
        }
    };
    match provider.repo_infos(&ws).await {
        Ok(repos) => r.pass(
            3,
            format!(
                "{} workspace(s) visible; {} repositories you are a member of in {ws}",
                workspaces.len(),
                repos.len()
            ),
        ),
        Err(e) => r.fail(
            3,
            format!(
                "{} workspace(s) visible, but the repositories of {ws} could not be listed: {}",
                workspaces.len(),
                e.user_message()
            ),
            api_hint(&e, SCOPE_READ_REPO),
        ),
    }
    workspaces
}

/// Check 4.
pub(super) async fn check_open_prs(
    r: &mut Report,
    provider: &BitbucketProvider,
    args: &CheckArgs,
    workspaces: &[String],
) -> Found {
    if let Some(repo) = &args.repo {
        let mut first = None;
        match provider.prs(repo, PrState::Open, 0).await {
            Ok(list) => {
                first = list.prs.first().map(|p| p.id);
                let mut detail = format!(
                    "{} open pull request(s) in {} (from --repo)",
                    list.prs.len(),
                    repo.full_name()
                );
                if let (None, Some(id)) = (args.pr, first) {
                    detail.push_str(&format!("; using the first PR #{id}"));
                }
                r.pass(4, detail);
            }
            Err(e) => r.fail(
                4,
                format!(
                    "could not list the pull requests of {}: {}",
                    repo.full_name(),
                    e.user_message()
                ),
                api_hint(&e, SCOPE_READ_PR),
            ),
        }
        return Found {
            repo: Some(repo.clone()),
            first_pr: first,
            any_repo: None,
        };
    }
    let mut scanned = 0;
    let mut first_repo = None;
    let mut first_error: Option<ProviderError> = None;
    let mut ok = 0;
    'outer: for ws in workspaces {
        let repos = match provider.repo_infos(ws).await {
            Ok(repos) => repos.into_iter().map(|i| i.repo).collect::<Vec<_>>(),
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        for repo in repos {
            if scanned >= MAX_REPOS_SCANNED {
                break 'outer;
            }
            scanned += 1;
            first_repo.get_or_insert_with(|| repo.clone());
            match provider.prs(&repo, PrState::Open, 0).await {
                Ok(list) if !list.prs.is_empty() => {
                    r.pass(
                        4,
                        format!(
                            "picked {}: the first repository with open pull requests ({} open, \
                             {scanned} repositories asked); using the first PR #{}",
                            repo.full_name(),
                            list.prs.len(),
                            list.prs[0].id
                        ),
                    );
                    return Found {
                        first_pr: Some(list.prs[0].id),
                        repo: Some(repo),
                        any_repo: first_repo,
                    };
                }
                Ok(_) => ok += 1,
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
    }
    match (ok, first_error) {
        (0, Some(e)) => r.fail(
            4,
            format!("could not list pull requests: {}", e.user_message()),
            api_hint(&e, SCOPE_READ_PR),
        ),
        (0, None) => r.skip(4, "no repository found to look in"),
        (n, _) => r.pass(
            4,
            format!(
                "listed pull requests in {n} repositories, none is open (asked at most \
                 {MAX_REPOS_SCANNED}); pass --repo <workspace>/<repo> --pr <id> to read one"
            ),
        ),
    }
    Found {
        repo: None,
        first_pr: None,
        any_repo: first_repo,
    }
}

pub(super) async fn check_read_pr(
    r: &mut Report,
    provider: &BitbucketProvider,
    client: &BbClient,
    repo: &RepoRef,
    id: u64,
) {
    let diff_path = format!("{}/diff", pr_path(repo, id));
    let (stat, diff, comments) = tokio::join!(
        provider.diffstat(repo, id),
        client.get_text(&diff_path),
        provider.comments(repo, id),
    );
    let mut parts = Vec::new();
    let mut failed: Vec<(&str, ProviderError)> = Vec::new();
    match stat {
        Ok(s) => parts.push(format!(
            "diffstat {} file(s) +{} -{}",
            s.files, s.lines_added, s.lines_removed
        )),
        Err(e) => failed.push(("diffstat", e)),
    }
    match diff {
        Ok(d) => parts.push(format!("diff {} KB", d.len().div_ceil(1024))),
        Err(e) => failed.push(("diff", e)),
    }
    match comments {
        Ok(c) => {
            let bots = c.comments.iter().filter(|c| c.author_is_bot).count();
            let tasks = match &c.tasks {
                Some(t) => format!("{} task(s)", t.len()),
                None => "tasks not readable".into(),
            };
            parts.push(format!(
                "{} comment(s), {bots} of them from bots / apps, {tasks}",
                c.comments.len()
            ));
        }
        Err(e) => failed.push(("comments", e)),
    }
    let what = format!("{} PR #{id}", repo.full_name());
    if failed.is_empty() {
        r.pass(5, format!("{what}: {}", parts.join("; ")));
        return;
    }
    let errs: Vec<String> = failed
        .iter()
        .map(|(p, e)| format!("{p} ({})", e.user_message()))
        .collect();
    let mut detail = format!("{what}: could not read {}", errs.join(", "));
    if !parts.is_empty() {
        detail.push_str(&format!("; read: {}", parts.join("; ")));
    }
    r.fail(5, detail, api_hint(&failed[0].1, SCOPE_READ_PR));
}

pub(super) async fn check_approvals(
    r: &mut Report,
    provider: &BitbucketProvider,
    client: &BbClient,
    repo: &RepoRef,
    id: u64,
) {
    use prcore::model::ReviewState;
    let pr = match provider.pr(repo, id).await {
        Ok(pr) => pr,
        Err(e) => {
            r.fail(
                6,
                format!("could not read PR #{id}: {}", e.user_message()),
                api_hint(&e, SCOPE_READ_PR),
            );
            return;
        }
    };
    let count = |s: ReviewState| pr.reviewers.iter().filter(|x| x.state == s).count();
    let mut detail = format!(
        "{} reviewer(s): {} approved, {} requested changes, {} pending",
        pr.reviewers.len(),
        count(ReviewState::Approved),
        count(ReviewState::ChangesRequested),
        count(ReviewState::Pending)
    );
    let q = format!("repository.full_name=\"{}\"", repo.full_name());
    let path = format!("/user/permissions/repositories?q={}", percent_encode(&q));
    match client.get_json(&path).await {
        Ok(v) => match v["values"][0]["permission"].as_str() {
            Some("read") => detail.push_str(
                "; your permission on the repository: read (Bitbucket may refuse your approvals \
                 and comments)",
            ),
            Some(p) => detail.push_str(&format!("; your permission on the repository: {p}")),
            None => detail.push_str("; your permission on the repository is not listed"),
        },
        Err(e) => detail.push_str(&format!(
            "; your permission level could not be read ({}; nit does not need it)",
            e.user_message()
        )),
    }
    r.pass(6, detail);
}

pub(super) fn check_git_auth(
    r: &mut Report,
    git: &dyn GitSide,
    creds: &Creds,
    me: Option<&Me>,
    repo: &RepoRef,
) {
    let url = format!("https://bitbucket.org/{}/{}.git", repo.namespace, repo.slug);
    let auth = GitAuth::bitbucket(&creds.token);
    let err = match git.ls_remote(&url, None, &auth) {
        Ok(()) => {
            return r.pass(
                7,
                format!(
                    "git ls-remote of {url} works with the user name {GIT_TOKEN_USER} (the one \
                     nit uses)"
                ),
            )
        }
        Err(e) => e,
    };
    let kind = classify_git(&err);
    let said = first_line(&err);
    match kind {
        GitFailure::Cert => {
            return r.fail(
                7,
                format!("git could not verify the certificate of bitbucket.org: {said}"),
                Some(
                    "git has its own certificate settings: run `git config --global \
                     http.sslBackend schannel` to use the Windows certificate store (or ask IT \
                     for the company root and set http.sslCAInfo)"
                        .into(),
                ),
            )
        }
        GitFailure::Network | GitFailure::Timeout => {
            return r.fail(
                7,
                format!("git could not reach bitbucket.org: {said}"),
                Some(
                    "git uses its own proxy settings: set `git config --global http.proxy \
                     http://<proxy>:<port>` (or HTTPS_PROXY) and check the connection"
                        .into(),
                ),
            )
        }
        _ => {}
    }
    // diagnosis only: which user name, if any, git accepts this token with
    let mut alternates: Vec<(&str, String)> = vec![("your account email", creds.email.clone())];
    if let Some(me) = me {
        if let Some(u) = &me.username {
            alternates.push(("your Bitbucket username", u.clone()));
        }
        if let Some(u) = &me.uuid {
            alternates.push(("your account UUID", u.clone()));
        }
    }
    let mut tried = Vec::new();
    let mut works = None;
    for (label, user) in &alternates {
        let ok = git.ls_remote(&url, Some(user), &auth).is_ok();
        tried.push(format!(
            "{label}: {}",
            if ok { "accepted" } else { "refused" }
        ));
        if ok && works.is_none() {
            works = Some(*label);
        }
    }
    let why = match kind {
        GitFailure::Auth => "git refused the token",
        GitFailure::NoAccess => "git was denied access to the repository",
        _ => "git ls-remote failed",
    };
    let detail = format!(
        "{why} for {url} with the user name {GIT_TOKEN_USER} (the one nit uses): {said}. \
         Also tried as user name only to diagnose: {}",
        tried.join(", ")
    );
    let hint = match works {
        Some(label) => format!(
            "git accepts this token only with {label} as user name, so it is probably not an \
             Atlassian API token with scopes (an app password or an older token type). Create an \
             API token with scopes (Bitbucket: read repository, read and write pull request) and \
             sign in again with it; if you are sure it is such a token, tell the nit \
             developers that git needs {label} as user name instead of {GIT_TOKEN_USER} \
             (GIT_TOKEN_USER in crates/gitlocal/src/runner.rs)"
        ),
        None if kind == GitFailure::NoAccess => format!(
            "check that your account can read {} and that the token has the scope repository: \
             read",
            repo.full_name()
        ),
        None => "git refused every user name: the token probably lacks the scope repository: \
                 read (needed for git) or is not valid; create a new API token with scopes and \
                 sign in again"
            .into(),
    };
    r.fail(7, detail, Some(hint));
}
