//! `--check-token` for a GitHub account (spec 5.3, 5.4): read-only checks of the token
//! through the same provider and git path the app uses. Nothing is written to GitHub
//! (GitHub is read-only in this version, so `--write-test` is skipped).
//!
//! Checks: 1 token present, 2 `GET /user` and the token kind (classic scopes from
//! `X-OAuth-Scopes`, else fine-grained), 3 list repositories, 4 open PRs of `--repo` (or the
//! first repositories found), 5 read one PR with its comments, 6 rate-limit headroom,
//! 7 `git ls-remote` with the account's git auth, 8 system git, 9 HTTPS reach.

use gitlocal::GITHUB_GIT_USER;
use prcore::model::{Forge, PrState, RepoRef};
use providers::github::{GitHubBases, GitHubProvider, SSO_HINT};
use providers::{Provider, ProviderError};
use store::{token_key, Account, SecretStore, CREDENTIAL_STORE};

use super::{
    check_git_version, check_reach, classify_git, first_line, CheckArgs, GitFailure, GitSide,
    Report, Source, Status,
};

/// Token for a first run without a stored GitHub account (and for CI).
pub const ENV_GITHUB_TOKEN: &str = "NIT_GITHUB_TOKEN";
/// The GitHub web address that token belongs to (default `https://github.com`).
pub const ENV_GITHUB_URL: &str = "NIT_GITHUB_URL";
/// The id of the account made from `NIT_GITHUB_TOKEN` / `NIT_GITHUB_URL`.
pub const ENV_ACCOUNT_ID: &str = "env-github";
/// At most this many repositories are asked for open PRs when no `--repo` is given (each
/// can cost a GraphQL query).
const MAX_REPOS_SCANNED: usize = 5;
/// Below this many requests left, check 6 fails.
const LOW_RATE: u64 = 100;

fn title(n: u8) -> &'static str {
    match n {
        1 => "Credentials present",
        2 => "REST sign-in (GET /user) and token kind",
        3 => "List repositories",
        4 => "List open pull requests",
        5 => "Read a pull request (comments and review threads)",
        6 => "Rate limit headroom",
        7 => "Git sign-in (git ls-remote)",
        8 => "System git",
        9 => "HTTPS / TLS / proxy to the API",
        _ => "",
    }
}

fn set(r: &mut Report, n: u8, status: Status, detail: impl Into<String>, hint: Option<String>) {
    r.checks.insert(
        n,
        super::Check {
            id: n.to_string(),
            title: title(n),
            status,
            detail: detail.into(),
            hint,
        },
    );
}

fn pass(r: &mut Report, n: u8, detail: impl Into<String>) {
    set(r, n, Status::Pass, detail, None);
}

fn fail(r: &mut Report, n: u8, detail: impl Into<String>, hint: Option<String>) {
    set(r, n, Status::Fail, detail, hint);
}

fn skip(r: &mut Report, n: u8, detail: impl Into<String>) {
    set(r, n, Status::Skip, detail, None);
}

/// The account `--account <wanted>` means when no stored account matches: one made from
/// `NIT_GITHUB_TOKEN` (and `NIT_GITHUB_URL`), when `wanted` is its host or `github`.
pub fn env_account(wanted: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Option<Account> {
    env(ENV_GITHUB_TOKEN).filter(|t| !t.trim().is_empty())?;
    let url = env(ENV_GITHUB_URL).unwrap_or_else(|| "https://github.com".into());
    let bases = providers::github::bases_for(&url).ok()?;
    let w = wanted?.trim().to_ascii_lowercase();
    (w == "github" || w == bases.git_host || w == ENV_ACCOUNT_ID).then(|| Account {
        id: prcore::model::AccountId(ENV_ACCOUNT_ID.into()),
        forge: Forge::GitHub,
        web_base: bases.web_base,
        api_base: bases.api_base,
        graphql_base: Some(bases.graphql_url),
        git_host: bases.git_host,
        user_id: String::new(),
        login: String::new(),
        display_name: String::new(),
        auth_kind: "pat".into(),
        enabled: true,
    })
}

/// The account's bases (the stored GraphQL address, else `<api>/graphql`).
pub fn bases_of(account: &Account) -> GitHubBases {
    GitHubBases {
        web_base: account.web_base.clone(),
        api_base: account.api_base.clone(),
        graphql_url: account
            .graphql_base
            .clone()
            .unwrap_or_else(|| format!("{}/graphql", account.api_base)),
        git_host: account.git_host.clone(),
    }
}

/// The token: the account's stored one, else `NIT_GITHUB_TOKEN`. `Err` is the reason (no
/// values).
pub fn load_token(
    secrets: &dyn SecretStore,
    account: &Account,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(String, Source), String> {
    let stored = secrets.get(&token_key(&account.id));
    if let Ok(Some(t)) = &stored {
        if !t.trim().is_empty() {
            return Ok((t.clone(), Source::Stored));
        }
    }
    if let Some(t) = env(ENV_GITHUB_TOKEN).filter(|t| !t.trim().is_empty()) {
        return Ok((t, Source::EnvGitHub));
    }
    Err(match stored {
        Err(_) => format!("could not read {CREDENTIAL_STORE}, and {ENV_GITHUB_TOKEN} is not set"),
        Ok(_) => format!(
            "no token saved for account {} in {CREDENTIAL_STORE}, and {ENV_GITHUB_TOKEN} is not set",
            account.id.as_str()
        ),
    })
}

/// What the checks need besides the token.
pub struct Inputs<'a> {
    pub args: &'a CheckArgs,
    pub bases: &'a GitHubBases,
    pub git: &'a dyn GitSide,
    pub proxy_var: Option<&'static str>,
}

const HINT_TOKEN: &str = "create a new personal access token (fine-grained: Pull requests: \
    read, Contents: read, Metadata: read on the repositories; or classic with the repo scope), \
    add the account again in Settings > Accounts, or set NIT_GITHUB_TOKEN and run again";

/// The organization named in an SSO authorization URL (`.../orgs/<org>/sso?...`).
fn sso_org(url: Option<&str>) -> Option<String> {
    let path = url?.split('?').next()?;
    let mut parts = path.split('/');
    parts.find(|p| *p == "orgs")?;
    parts.next().filter(|o| !o.is_empty()).map(str::to_string)
}

/// A hint for a failed API call. The SSO hint names the organization, never the
/// authorization URL (that is shown in the app only, as a link the user opens).
fn hint_for(e: &ProviderError, p: &GitHubProvider) -> Option<String> {
    match e {
        ProviderError::Auth => Some(HINT_TOKEN.into()),
        ProviderError::Rejected {
            status: 403,
            message,
        } if message == SSO_HINT => {
            let org = sso_org(p.client().sso_url().as_deref()).map_or_else(
                || "the organization".to_string(),
                |o| format!("the organization {o}"),
            );
            Some(format!(
                "authorize this token for {org} (GitHub: Settings, Developer settings, \
                 Personal access tokens, Configure SSO), then run again"
            ))
        }
        ProviderError::Forbidden => Some(match p.client().accepted_permissions() {
            Some(perm) => format!(
                "the token lacks a permission: GitHub says this call needs {perm}. Give the \
                 fine-grained token that permission on the repository (or use a classic token \
                 with the repo scope)"
            ),
            None => "the token was accepted but may not read this: check its repository access \
                     and permissions (an organization may need to approve fine-grained tokens)"
                .into(),
        }),
        ProviderError::RateLimited => Some(
            "GitHub's rate limit is reached; wait until it resets (see check 6) and run again"
                .into(),
        ),
        ProviderError::NotFound => Some(
            "not found: check the name, and that the token can see the repository (private \
             repositories are reported as not found without access)"
                .into(),
        ),
        _ => None,
    }
}

/// Runs every check; never writes anything to GitHub.
pub async fn run(inp: &Inputs<'_>, token: Result<(String, Source), String>) -> Report {
    let mut r = Report::default();
    // 8 and 9 have the same titles as the Bitbucket checks: shared code
    let git_ok = check_git_version(&mut r, inp.git);
    let (token, source) = match token {
        Ok(t) => t,
        Err(why) => {
            fail(&mut r, 1, why, Some(HINT_TOKEN.into()));
            for n in [2, 3, 4, 5, 6, 7, 9] {
                skip(&mut r, n, "needs a token (check 1)");
            }
            return r;
        }
    };
    r.source = Some(source);
    pass(&mut r, 1, format!("found in {}", source.label()));
    let b = inp.bases;
    let p = GitHubProvider::new(&b.api_base, &b.graphql_url, &b.git_host, &token);

    // 2 and 9: one authenticated request answers both
    let me = p.me().await;
    // check 9 only tells transport failures from any HTTP answer
    let reach = match &me {
        Err(ProviderError::Network(m)) => Err(ProviderError::Network(m.clone())),
        _ => Ok(serde_json::Value::Null),
    };
    check_reach(
        &mut r,
        &reach,
        &b.api_base,
        &host_of(&b.api_base),
        inp.proxy_var,
    );
    let me = match me {
        Ok(me) => {
            let who = me.login.clone().unwrap_or(me.display_name.clone());
            match p.client().oauth_scopes() {
                Some(s) if has_scope(&s, "repo") => pass(
                    &mut r,
                    2,
                    format!("signed in as {who}; classic token with scopes [{s}]"),
                ),
                Some(s) => fail(
                    &mut r,
                    2,
                    format!(
                        "signed in as {who}; classic token without the repo scope (scopes: [{s}])"
                    ),
                    Some(
                        "a classic token needs the repo scope to read private repositories and \
                         their pull requests; edit the token or create one with repo"
                            .into(),
                    ),
                ),
                None => pass(
                    &mut r,
                    2,
                    format!(
                        "signed in as {who}; fine-grained token (its permissions are per \
                         repository: checks 3 to 5 and 7 show what it may read)"
                    ),
                ),
            }
            true
        }
        Err(ProviderError::Network(_)) => {
            skip(&mut r, 2, "the API could not be reached (check 9)");
            false
        }
        Err(e) => {
            let detail = match &e {
                ProviderError::Auth => "GitHub refused the token (HTTP 401)".to_string(),
                e => format!("GET /user failed: {}", e.user_message()),
            };
            fail(&mut r, 2, detail, hint_for(&e, &p));
            false
        }
    };

    let mut found_repo: Option<RepoRef> = inp.args.repo.clone();
    let mut any_repo: Option<RepoRef> = None;
    let mut first_pr: Option<u64> = None;
    if me {
        // 3: repositories
        let candidates: Vec<RepoRef> = match p.discover().await {
            Ok(d) => {
                pass(
                    &mut r,
                    3,
                    format!(
                        "{} repositories of {} owners",
                        d.repos.len(),
                        d.namespaces.len()
                    ),
                );
                d.repos.into_iter().map(|i| i.repo).collect()
            }
            Err(e) => {
                fail(
                    &mut r,
                    3,
                    format!("GET /user/repos failed: {}", e.user_message()),
                    hint_for(&e, &p),
                );
                Vec::new()
            }
        };
        any_repo = candidates.first().cloned();
        // 4: open PRs of the given repository, else of the first few found
        let to_scan: Vec<RepoRef> = match &inp.args.repo {
            Some(r) => vec![r.clone()],
            None => candidates.into_iter().take(MAX_REPOS_SCANNED).collect(),
        };
        let mut last_err = None;
        let mut scanned = 0;
        for repo in &to_scan {
            scanned += 1;
            match p.prs(repo, PrState::Open, usize::MAX).await {
                Ok(list) if !list.prs.is_empty() || inp.args.repo.is_some() => {
                    let skipped = if list.skipped.is_empty() {
                        String::new()
                    } else {
                        format!(", {} could not be read", list.skipped.len())
                    };
                    pass(
                        &mut r,
                        4,
                        format!(
                            "{}: {} open pull request(s){skipped}",
                            repo.full_name(),
                            list.prs.len()
                        ),
                    );
                    found_repo = Some(repo.clone());
                    first_pr = list.prs.first().map(|p| p.id);
                    last_err = None;
                    break;
                }
                Ok(_) => {}
                Err(e) => last_err = Some((repo.clone(), e)),
            }
        }
        if !r.checks.contains_key(&4) {
            match last_err {
                Some((repo, e)) => fail(
                    &mut r,
                    4,
                    format!("{}: {}", repo.full_name(), e.user_message()),
                    hint_for(&e, &p),
                ),
                None if scanned == 0 => skip(&mut r, 4, "no repository to try: pass --repo <owner>/<repo>"),
                None => skip(
                    &mut r,
                    4,
                    format!("no open pull request in the first {scanned} repositories: pass --repo <owner>/<repo>"),
                ),
            }
        }
        // 5: one PR with its comments
        match (found_repo.clone(), inp.args.pr.or(first_pr)) {
            (Some(repo), Some(id)) => match p.pr(&repo, id).await {
                Ok(pr) => match p.comments(&repo, id).await {
                    Ok(c) => {
                        let threads = c.comments.iter().filter(|c| c.thread.is_some()).count();
                        let outdated = c
                            .comments
                            .iter()
                            .filter(|c| c.anchor.as_ref().is_some_and(|a| a.outdated))
                            .count();
                        pass(
                            &mut r,
                            5,
                            format!(
                                "{} #{id} \"{}\": {} comment(s), {threads} in review threads, {outdated} outdated",
                                repo.full_name(),
                                pr.title,
                                c.comments.len()
                            ),
                        )
                    }
                    Err(e) => fail(
                        &mut r,
                        5,
                        format!("reading the comments of #{id} failed: {}", e.user_message()),
                        hint_for(&e, &p),
                    ),
                },
                Err(e) => fail(
                    &mut r,
                    5,
                    format!(
                        "reading {} #{id} failed: {}",
                        repo.full_name(),
                        e.user_message()
                    ),
                    hint_for(&e, &p),
                ),
            },
            _ => skip(
                &mut r,
                5,
                "no pull request to read: pass --repo <owner>/<repo> --pr <number>",
            ),
        }
    } else {
        for n in [3, 4, 5] {
            skip(&mut r, n, "needs a working REST sign-in (check 2)");
        }
    }

    // 6: what the last answer said about the primary limit
    match p.client().rate_limit() {
        Some((left, limit)) if left < LOW_RATE => fail(
            &mut r,
            6,
            format!("{left} of {limit} requests left this hour"),
            Some("wait for the hourly reset before syncing many repositories".into()),
        ),
        Some((left, limit)) => pass(&mut r, 6, format!("{left} of {limit} requests left this hour")),
        None => skip(
            &mut r,
            6,
            "the server sent no rate-limit headers (GitHub Enterprise with rate limits off, or no answer)",
        ),
    }

    // 7: git, with the account's own git auth (user name x-access-token, host-scoped)
    match (inp.args.repo.clone().or(found_repo).or(any_repo), git_ok) {
        (_, false) => skip(&mut r, 7, "needs a working git (check 8)"),
        (None, true) => skip(
            &mut r,
            7,
            "no repository to try: pass --repo <owner>/<repo>",
        ),
        (Some(repo), true) => check_git(&mut r, inp.git, &token, b, &repo),
    }

    if inp.args.write_test {
        r.write_target = inp
            .args
            .repo
            .as_ref()
            .map(|repo| format!("{} PR #{}", repo.full_name(), inp.args.pr.unwrap_or(0)));
        r.step(
            1,
            "Post a PR comment",
            Status::Skip,
            "not attempted: the write test is not available for GitHub yet".into(),
            None,
        );
    }
    r
}

fn has_scope(scopes: &str, want: &str) -> bool {
    scopes.split(',').any(|s| s.trim() == want)
}

fn host_of(base: &str) -> String {
    base.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(base)
        .to_string()
}

fn check_git(r: &mut Report, git: &dyn GitSide, token: &str, b: &GitHubBases, repo: &RepoRef) {
    let host = b.git_host.as_str();
    // plain http only to the account's own private origin (none for any other host)
    let Some(auth) = crate::backend::forge_git_auth(GITHUB_GIT_USER, token, &b.web_base, host)
    else {
        return fail(
            r,
            7,
            format!("{host} is plain http on a public address; git was not run"),
            None,
        );
    };
    let url = format!(
        "{}://{host}/{}/{}.git",
        auth.scheme(),
        repo.namespace,
        repo.slug
    );
    let err = match git.ls_remote(&url, None, &auth) {
        Ok(()) => {
            return pass(
                r,
                7,
                format!("git ls-remote of {url} works with the user name {GITHUB_GIT_USER} (the one nit uses)"),
            )
        }
        Err(e) => e,
    };
    let said = first_line(&err);
    let (detail, hint) = match classify_git(&err) {
        GitFailure::Cert => (
            format!("git could not verify the certificate of {host}: {said}"),
            "git has its own certificate settings: run `git config --global http.sslBackend \
             schannel` to use the Windows certificate store (or ask IT for the company root and \
             set http.sslCAInfo)"
                .to_string(),
        ),
        GitFailure::Network | GitFailure::Timeout => (
            format!("git could not reach {host}: {said}"),
            "git uses its own proxy settings: set `git config --global http.proxy \
             http://<proxy>:<port>` (or HTTPS_PROXY) and check the connection"
                .to_string(),
        ),
        GitFailure::Auth => (
            format!("git refused the token for {url}: {said}"),
            "a fine-grained token needs Contents: read on this repository, a classic token the \
             repo scope; with SAML single sign-on the token must be authorized for the \
             organization. GitHub checks the token even for public repositories, so an expired \
             one fails everywhere"
                .to_string(),
        ),
        GitFailure::NoAccess => (
            format!("git was denied access to {url}: {said}"),
            format!("check that the token can read {}", repo.full_name()),
        ),
        GitFailure::Other => (
            format!("git ls-remote of {url} failed: {said}"),
            String::new(),
        ),
    };
    fail(r, 7, detail, (!hint.is_empty()).then_some(hint));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitlocal::GitAuth;
    use serde_json::json;
    use std::cell::RefCell;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "gh-very-secret-token-0123456789";

    struct FakeGit {
        ok: bool,
        error: String,
        urls: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeGit {
        fn ok() -> FakeGit {
            FakeGit {
                ok: true,
                error: String::new(),
                urls: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitSide for FakeGit {
        fn version(&self) -> Result<String, String> {
            Ok("git version 2.47.1.windows.1".into())
        }
        fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
            assert_eq!(auth.token, TOKEN);
            assert_eq!(user, None);
            self.urls
                .borrow_mut()
                .push((url.into(), auth.user.clone(), auth.scope.clone()));
            if self.ok {
                Ok(())
            } else {
                Err(self.error.clone())
            }
        }
    }

    fn bases(s: &MockServer) -> GitHubBases {
        GitHubBases {
            web_base: s.uri(),
            api_base: s.uri(),
            graphql_url: format!("{}/graphql", s.uri()),
            git_host: "github.com".into(),
        }
    }

    fn args(repo: Option<&str>, pr: Option<u64>) -> CheckArgs {
        CheckArgs {
            account: None,
            repo: repo.and_then(|r| RepoRef::parse("github.com", r)),
            pr,
            write_test: false,
        }
    }

    async fn user(s: &MockServer, scopes: Option<&str>) {
        let mut resp = ResponseTemplate::new(200)
            .set_body_json(json!({"login": "octocat", "id": 1, "name": "Mona"}))
            .insert_header("x-ratelimit-remaining", "4990")
            .insert_header("x-ratelimit-limit", "5000");
        if let Some(sc) = scopes {
            resp = resp.insert_header("x-oauth-scopes", sc);
        }
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(resp)
            .mount(s)
            .await;
    }

    async fn repos(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/user/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!([{"full_name": "octo-org/api", "name": "api", "private": true}]),
            ))
            .mount(s)
            .await;
    }

    async fn run_with(s: &MockServer, git: &FakeGit, a: &CheckArgs) -> Report {
        let b = bases(s);
        let inp = Inputs {
            args: a,
            bases: &b,
            git,
            proxy_var: None,
        };
        run(&inp, Ok((TOKEN.into(), Source::EnvGitHub))).await
    }

    #[tokio::test]
    async fn classic_without_repo_scope_fails_with_hint() {
        let s = MockServer::start().await;
        user(&s, Some("read:org, gist")).await;
        repos(&s).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&s)
            .await;
        let r = run_with(&s, &FakeGit::ok(), &args(None, None)).await;
        let c = &r.checks[&2];
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("without the repo scope"), "{}", c.detail);
        assert!(c.hint.as_deref().unwrap().contains("repo scope"));
        // the other checks still ran
        assert_eq!(r.checks[&3].status, Status::Pass);
        assert_eq!(r.checks[&6].status, Status::Pass);
        assert_eq!(r.checks[&6].detail, "4990 of 5000 requests left this hour");
    }

    #[tokio::test]
    async fn fine_grained_token_passes_and_git_uses_x_access_token_on_the_host() {
        let s = MockServer::start().await;
        user(&s, None).await;
        repos(&s).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&s)
            .await;
        let git = FakeGit::ok();
        let r = run_with(&s, &git, &args(Some("octo-org/api"), None)).await;
        assert_eq!(r.checks[&2].status, Status::Pass);
        assert!(r.checks[&2].detail.contains("fine-grained"));
        assert_eq!(r.checks[&4].status, Status::Pass, "{}", r.checks[&4].detail);
        assert_eq!(r.checks[&7].status, Status::Pass);
        assert_eq!(
            git.urls.borrow()[0],
            (
                "https://github.com/octo-org/api.git".to_string(),
                "x-access-token".to_string(),
                "https://github.com/".to_string()
            )
        );
        let text = r.render(&[TOKEN]);
        assert!(!text.contains(TOKEN));
        assert!(text.contains("List open pull requests"));
    }

    #[tokio::test]
    async fn fine_grained_403_shows_accepted_permissions() {
        let s = MockServer::start().await;
        user(&s, None).await;
        repos(&s).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(
                        json!({"message": "Resource not accessible by personal access token"}),
                    )
                    .insert_header("x-accepted-github-permissions", "pull_requests=read"),
            )
            .mount(&s)
            .await;
        let r = run_with(&s, &FakeGit::ok(), &args(Some("octo-org/api"), None)).await;
        let c = &r.checks[&4];
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.hint.as_deref().unwrap().contains("pull_requests=read"),
            "{:?}",
            c.hint
        );
    }

    #[tokio::test]
    async fn sso_hint_includes_org_not_url_in_report_file() {
        let s = MockServer::start().await;
        user(&s, Some("repo")).await;
        repos(&s).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"message": "Resource protected by organization SAML enforcement."}))
                    .insert_header(
                        "x-github-sso",
                        "required; url=https://github.com/orgs/octo-org/sso?authorization_request=SECRETISH",
                    ),
            )
            .mount(&s)
            .await;
        let r = run_with(&s, &FakeGit::ok(), &args(Some("octo-org/api"), None)).await;
        let text = r.render(&[TOKEN]);
        assert!(text.contains("the organization octo-org"), "{text}");
        assert!(!text.contains("authorization_request"), "{text}");
        assert!(!text.contains("https://github.com/orgs"), "{text}");
    }

    #[tokio::test]
    async fn a_pr_is_read_with_its_comments_and_write_test_is_skipped() {
        let s = MockServer::start().await;
        user(&s, Some("repo")).await;
        repos(&s).await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 7, "title": "Fix", "state": "open",
                "user": {"login": "hubot", "id": 2},
                "head": {"ref": "f", "sha": "a"}, "base": {"ref": "main", "sha": "b"},
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
            })))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviewThreads"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository":
                {"pullRequest": {"reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviews("))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"repository":
                {"pullRequest": {"reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}})),
            )
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/octo-org/api/issues/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 1, "body": "hi", "user": {"login": "hubot", "id": 2},
                 "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}
            ])))
            .mount(&s)
            .await;
        let mut a = args(Some("octo-org/api"), Some(7));
        a.write_test = true;
        let r = run_with(&s, &FakeGit::ok(), &a).await;
        assert_eq!(r.checks[&5].status, Status::Pass, "{}", r.checks[&5].detail);
        assert!(r.checks[&5].detail.contains("1 comment(s)"));
        assert_eq!(r.write[0].status, Status::Skip);
        assert!(r.write[0].detail.contains("not available"));
        // nothing but GETs and GraphQL reads were sent
        for req in s.received_requests().await.unwrap() {
            let read = req.method.as_str() == "GET" || req.url.path() == "/graphql";
            assert!(read, "{} {}", req.method, req.url);
        }
    }

    #[tokio::test]
    async fn a_refused_token_skips_the_rest_and_git_auth_failure_has_a_hint() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
            )
            .mount(&s)
            .await;
        let git = FakeGit {
            ok: false,
            error: "fatal: Authentication failed for 'https://github.com/octo-org/api.git/'".into(),
            urls: RefCell::new(Vec::new()),
        };
        let r = run_with(&s, &git, &args(Some("octo-org/api"), None)).await;
        assert_eq!(r.checks[&2].status, Status::Fail);
        assert_eq!(r.checks[&3].status, Status::Skip);
        assert_eq!(r.checks[&7].status, Status::Fail);
        assert!(r.checks[&7]
            .hint
            .as_deref()
            .unwrap()
            .contains("Contents: read"));
    }

    #[test]
    fn the_env_account_needs_the_token_and_a_matching_name() {
        let env = |k: &str| match k {
            ENV_GITHUB_TOKEN => Some("t".to_string()),
            ENV_GITHUB_URL => Some("https://ghe.corp.example".to_string()),
            _ => None,
        };
        let a = env_account(Some("ghe.corp.example"), env).unwrap();
        assert_eq!(a.api_base, "https://ghe.corp.example/api/v3");
        assert!(env_account(Some("github"), env).is_some());
        assert!(env_account(Some("bitbucket.org"), env).is_none());
        assert!(env_account(Some("github.com"), |_| None).is_none());
        let plain = |k: &str| (k == ENV_GITHUB_TOKEN).then(|| "t".to_string());
        assert_eq!(
            env_account(Some("github.com"), plain).unwrap().git_host,
            "github.com"
        );
    }

    #[test]
    fn the_token_comes_from_the_account_else_the_environment() {
        let secrets = store::MemorySecrets::new();
        let a = env_account(Some("github"), |k| {
            (k == ENV_GITHUB_TOKEN).then(|| "x".into())
        })
        .unwrap();
        let none = |_: &str| None;
        assert!(load_token(&secrets, &a, none)
            .unwrap_err()
            .contains(ENV_GITHUB_TOKEN));
        secrets.set(&token_key(&a.id), "stored").unwrap();
        assert_eq!(
            load_token(&secrets, &a, none).unwrap(),
            ("stored".into(), Source::Stored)
        );
    }

    #[test]
    fn sso_org_is_read_from_the_path_only() {
        assert_eq!(
            sso_org(Some(
                "https://github.com/orgs/octo-org/sso?authorization_request=x"
            ))
            .as_deref(),
            Some("octo-org")
        );
        assert_eq!(sso_org(Some("https://github.com/sso")), None);
        assert_eq!(sso_org(None), None);
    }
}
