//! `--check-token` for a GitLab account (spec 5.3, 5.4): read-only checks of the token
//! through the same provider and git path the app uses. Nothing is written to GitLab
//! (there is no GitLab `--write-test` in this version; it is skipped).
//!
//! Checks: 1 token present, 2 `GET /user`, 3 list projects, 4 open MRs of `--repo` (or the
//! first projects found), 5 read one MR with its discussions, 6 token scopes and expiry
//! (`GET /personal_access_tokens/self`), 7 `git ls-remote` as `oauth2` on the account's
//! host, 8 system git, 9 HTTPS reach of the API. API and git TLS are reported separately
//! (9 and 7): git has its own certificate settings.

use chrono::NaiveDate;
use gitlocal::GITLAB_GIT_USER;
use prcore::model::{Forge, PrState, RepoRef};
use providers::gitlab::{GitLabBases, GitLabProvider, TokenInfo};
use providers::{Provider, ProviderError};
use store::{token_key, Account, SecretStore, CREDENTIAL_STORE};

use super::{
    check_git_version, check_reach, classify_git, first_line, CheckArgs, GitFailure, GitSide,
    Report, Source, Status,
};

/// Token for a first run without a stored GitLab account (and for CI).
pub const ENV_GITLAB_TOKEN: &str = "NIT_GITLAB_TOKEN";
/// The GitLab web address that token belongs to (default `https://gitlab.com`).
pub const ENV_GITLAB_URL: &str = "NIT_GITLAB_URL";
/// The id of the account made from `NIT_GITLAB_TOKEN` / `NIT_GITLAB_URL`.
pub const ENV_ACCOUNT_ID: &str = "env-gitlab";
/// At most this many projects are asked for open MRs when no `--repo` is given.
const MAX_REPOS_SCANNED: usize = 5;
/// A token expiring within this many days is warned about (spec 5.3).
pub const EXPIRY_WARNING_DAYS: i64 = 14;

fn title(n: u8) -> &'static str {
    match n {
        1 => "Credentials present",
        2 => "REST sign-in (GET /user)",
        3 => "List projects",
        4 => "List open merge requests",
        5 => "Read a merge request (discussions)",
        6 => "Token scopes and expiry",
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

/// A warning when the token expires within `EXPIRY_WARNING_DAYS` of `today` (or already
/// did); None for a token without expiry or one that lasts longer.
pub fn expiry_warning(expires: Option<NaiveDate>, today: NaiveDate) -> Option<String> {
    let days = (expires? - today).num_days();
    match days {
        d if d < 0 => Some(format!("the token expired on {}", expires?)),
        0 => Some("the token expires today".into()),
        d if d <= EXPIRY_WARNING_DAYS => {
            Some(format!("the token expires in {d} day(s), on {}", expires?))
        }
        _ => None,
    }
}

/// What the token's scopes allow, as a note for the user; `Err` when the app cannot work
/// with them (spec 5.3: `read_api` reads, `api` also comments and approves; git needs
/// `read_repository` without `api`).
pub fn scope_note(scopes: &[String]) -> Result<String, String> {
    let has = |s: &str| scopes.iter().any(|x| x == s);
    let list = if scopes.is_empty() {
        "none".to_string()
    } else {
        scopes.join(", ")
    };
    if !has("api") && !has("read_api") {
        return Err(format!(
            "scopes [{list}]: nit needs read_api (or api) to read projects and merge requests"
        ));
    }
    if !has("api") && !has("read_repository") {
        return Err(format!(
            "scopes [{list}]: git fetch needs read_repository (or api) next to read_api"
        ));
    }
    if has("api") {
        Ok(format!(
            "scopes [{list}]: reads, git fetch, comments and approvals"
        ))
    } else {
        Ok(format!(
            "scopes [{list}]: read-only; comments and approvals need the api scope"
        ))
    }
}

/// The account `--account <wanted>` means when no stored account matches: one made from
/// `NIT_GITLAB_TOKEN` (and `NIT_GITLAB_URL`), when `wanted` is its host or `gitlab`.
pub fn env_account(wanted: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Option<Account> {
    env(ENV_GITLAB_TOKEN).filter(|t| !t.trim().is_empty())?;
    let url = env(ENV_GITLAB_URL).unwrap_or_else(|| providers::gitlab::GITLAB_COM.into());
    let bases = providers::gitlab::bases_for(&url).ok()?;
    let w = wanted?.trim().to_ascii_lowercase();
    (w == "gitlab" || w == bases.git_host || w == ENV_ACCOUNT_ID).then(|| Account {
        id: prcore::model::AccountId(ENV_ACCOUNT_ID.into()),
        forge: Forge::GitLab,
        web_base: bases.web_base,
        api_base: bases.api_base,
        graphql_base: None,
        git_host: bases.git_host,
        user_id: String::new(),
        login: String::new(),
        display_name: String::new(),
        auth_kind: "pat".into(),
        enabled: true,
    })
}

/// The account's bases.
pub fn bases_of(account: &Account) -> GitLabBases {
    GitLabBases {
        web_base: account.web_base.clone(),
        api_base: account.api_base.clone(),
        git_host: account.git_host.clone(),
    }
}

/// The token: the account's stored one, else `NIT_GITLAB_TOKEN`. `Err` is the reason.
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
    if let Some(t) = env(ENV_GITLAB_TOKEN).filter(|t| !t.trim().is_empty()) {
        return Ok((t, Source::EnvGitLab));
    }
    Err(match stored {
        Err(_) => format!("could not read {CREDENTIAL_STORE}, and {ENV_GITLAB_TOKEN} is not set"),
        Ok(_) => format!(
            "no token saved for account {} in {CREDENTIAL_STORE}, and {ENV_GITLAB_TOKEN} is not set",
            account.id.as_str()
        ),
    })
}

/// What the checks need besides the token.
pub struct Inputs<'a> {
    pub args: &'a CheckArgs,
    pub bases: &'a GitLabBases,
    pub git: &'a dyn GitSide,
    pub proxy_var: Option<&'static str>,
    /// Today's date, for the expiry warning.
    pub today: NaiveDate,
}

const HINT_TOKEN: &str = "create a new personal access token with the read_api and \
    read_repository scopes (or api), add the account again in Settings > Accounts, or set \
    NIT_GITLAB_TOKEN and run again";

fn hint_for(e: &ProviderError) -> Option<String> {
    match e {
        ProviderError::Auth => Some(HINT_TOKEN.into()),
        ProviderError::Forbidden => Some(
            "the token was accepted but may not read this: check its scopes (read_api) and \
             your role in the project"
                .into(),
        ),
        ProviderError::RateLimited => {
            Some("GitLab is rate-limiting this account; wait a minute and run again".into())
        }
        ProviderError::NotFound => Some(
            "not found: check the path (group/subgroup/project), and that the token can see \
             the project (private projects are reported as not found without access)"
                .into(),
        ),
        _ => None,
    }
}

/// Runs every check; never writes anything to GitLab.
pub async fn run(inp: &Inputs<'_>, token: Result<(String, Source), String>) -> Report {
    let mut r = Report::default();
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
    let p = GitLabProvider::new(&b.api_base, &b.git_host, &token);

    let me = p.me().await;
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
    let signed_in = match me {
        Ok(me) => {
            let who = me.login.clone().unwrap_or(me.display_name.clone());
            pass(&mut r, 2, format!("signed in as {who}"));
            true
        }
        Err(ProviderError::Network(_)) => {
            skip(&mut r, 2, "the API could not be reached (check 9)");
            false
        }
        Err(e) => {
            let detail = match &e {
                ProviderError::Auth => "GitLab refused the token (HTTP 401)".to_string(),
                e => format!("GET /user failed: {}", e.user_message()),
            };
            fail(&mut r, 2, detail, hint_for(&e));
            false
        }
    };

    let mut found_repo: Option<RepoRef> = inp.args.repo.clone();
    let mut any_repo: Option<RepoRef> = None;
    if signed_in {
        let candidates: Vec<RepoRef> = match p.discover().await {
            Ok(d) => {
                pass(
                    &mut r,
                    3,
                    format!(
                        "{} projects in {} namespaces",
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
                    format!("GET /projects failed: {}", e.user_message()),
                    hint_for(&e),
                );
                Vec::new()
            }
        };
        any_repo = candidates.first().cloned();
        let to_scan: Vec<RepoRef> = match &inp.args.repo {
            Some(r) => vec![r.clone()],
            None => candidates.into_iter().take(MAX_REPOS_SCANNED).collect(),
        };
        let mut first_mr = None;
        let mut last_err = None;
        for repo in &to_scan {
            match p.prs(repo, PrState::Open, usize::MAX).await {
                Ok(list) if !list.prs.is_empty() || inp.args.repo.is_some() => {
                    pass(
                        &mut r,
                        4,
                        format!(
                            "{}: {} open merge request(s)",
                            repo.full_name(),
                            list.prs.len()
                        ),
                    );
                    found_repo = Some(repo.clone());
                    first_mr = list.prs.first().map(|p| p.id);
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
                    hint_for(&e),
                ),
                None => skip(
                    &mut r,
                    4,
                    "no open merge request found: pass --repo <group/project>",
                ),
            }
        }
        match (found_repo.clone(), inp.args.pr.or(first_mr)) {
            (Some(repo), Some(id)) => match p.comments(&repo, id).await {
                Ok(c) => {
                    let outdated = c
                        .comments
                        .iter()
                        .filter(|c| c.anchor.as_ref().is_some_and(|a| a.outdated))
                        .count();
                    pass(
                        &mut r,
                        5,
                        format!(
                            "{} !{id}: {} note(s), {outdated} outdated",
                            repo.full_name(),
                            c.comments.len()
                        ),
                    )
                }
                Err(e) => fail(
                    &mut r,
                    5,
                    format!(
                        "reading the discussions of !{id} failed: {}",
                        e.user_message()
                    ),
                    hint_for(&e),
                ),
            },
            _ => skip(
                &mut r,
                5,
                "no merge request to read: pass --repo <group/project> --pr <iid>",
            ),
        }
        match p.token_info().await {
            Ok(info) => check_token_info(&mut r, &info, inp.today),
            Err(e) => skip(
                &mut r,
                6,
                format!(
                    "GET /personal_access_tokens/self failed ({}); not a personal access \
                     token, or an older GitLab",
                    e.user_message()
                ),
            ),
        }
    } else {
        for n in [3, 4, 5, 6] {
            skip(&mut r, n, "needs a working REST sign-in (check 2)");
        }
    }

    match (inp.args.repo.clone().or(found_repo).or(any_repo), git_ok) {
        (_, false) => skip(&mut r, 7, "needs a working git (check 8)"),
        (None, true) => skip(&mut r, 7, "no project to try: pass --repo <group/project>"),
        (Some(repo), true) => check_git(&mut r, inp.git, &token, b, &repo),
    }

    if inp.args.write_test {
        r.write_target = inp
            .args
            .repo
            .as_ref()
            .map(|repo| format!("{} MR !{}", repo.full_name(), inp.args.pr.unwrap_or(0)));
        r.step(
            1,
            "Post an MR comment",
            Status::Skip,
            "not attempted: there is no GitLab write test in this version".into(),
            None,
        );
    }
    r
}

fn check_token_info(r: &mut Report, info: &TokenInfo, today: NaiveDate) {
    if !info.active {
        return fail(
            r,
            6,
            "the token is not active (revoked or expired)",
            Some(HINT_TOKEN.into()),
        );
    }
    let scopes = scope_note(&info.scopes);
    let expiry = expiry_warning(info.expires_at, today);
    let when = match info.expires_at {
        Some(d) => format!("expires on {d}"),
        None => "no expiry date".into(),
    };
    match (scopes, expiry) {
        (Err(why), _) => fail(r, 6, format!("{why}; {when}"), Some(HINT_TOKEN.into())),
        (Ok(note), Some(warn)) => fail(
            r,
            6,
            format!("{note}; {warn}"),
            Some("create a new token before it expires and add it in Settings > Accounts".into()),
        ),
        (Ok(note), None) => pass(r, 6, format!("{note}; {when}")),
    }
}

fn host_of(base: &str) -> String {
    base.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(base)
        .to_string()
}

fn check_git(r: &mut Report, git: &dyn GitSide, token: &str, b: &GitLabBases, repo: &RepoRef) {
    let host = b.git_host.as_str();
    // plain http only to the account's own private origin (none for any other host)
    let Some(auth) = crate::backend::forge_git_auth(GITLAB_GIT_USER, token, &b.web_base, host)
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
                format!("git ls-remote of {url} works with the user name {GITLAB_GIT_USER} (the one nit uses)"),
            )
        }
        Err(e) => e,
    };
    let said = first_line(&err);
    let (detail, hint) = match classify_git(&err) {
        GitFailure::Cert => (
            format!("git could not verify the certificate of {host}: {said}"),
            "git does not use the API's certificate check (check 9): run `git config --global \
             http.sslBackend schannel` to use the Windows certificate store (or ask IT for the \
             company root and set http.sslCAInfo)"
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
            "git needs the read_repository scope (or api) on the token; GitLab checks the \
             token even for public projects, so an expired one fails everywhere"
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
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "glpat-very-secret-token-0123456789";

    struct FakeGit {
        result: Result<(), String>,
        calls: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeGit {
        fn ok() -> FakeGit {
            FakeGit {
                result: Ok(()),
                calls: RefCell::new(Vec::new()),
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
            self.calls
                .borrow_mut()
                .push((url.into(), auth.user.clone(), auth.scope.clone()));
            self.result.clone()
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()
    }

    fn bases(s: &MockServer, git_host: &str) -> GitLabBases {
        GitLabBases {
            web_base: s.uri(),
            api_base: s.uri(),
            git_host: git_host.into(),
        }
    }

    fn args(repo: Option<&str>) -> CheckArgs {
        CheckArgs {
            account: None,
            repo: repo.and_then(|r| RepoRef::parse("gitlab.com", r)),
            pr: None,
            write_test: false,
        }
    }

    async fn server(scopes: &[&str], expires: Option<&str>) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": 1, "username": "root", "name": "Administrator"})),
            )
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/personal_access_tokens/self"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id": 4, "scopes": scopes, "active": true, "expires_at": expires}),
            ))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 3, "path_with_namespace": "grp/sub/proj", "name": "proj"}
            ])))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"/merge_requests$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&s)
            .await;
        s
    }

    async fn run_with(s: &MockServer, git: &FakeGit, a: &CheckArgs, host: &str) -> Report {
        let b = bases(s, host);
        let inp = Inputs {
            args: a,
            bases: &b,
            git,
            proxy_var: None,
            today: today(),
        };
        run(&inp, Ok((TOKEN.into(), Source::EnvGitLab))).await
    }

    #[tokio::test]
    async fn read_api_only_token_warns_comments_disabled() {
        let s = server(&["read_api", "read_repository"], Some("2027-06-01")).await;
        let git = FakeGit::ok();
        let r = run_with(&s, &git, &args(Some("grp/sub/proj")), "gitlab.com").await;
        let c = &r.checks[&6];
        assert_eq!(c.status, Status::Pass, "{}", c.detail);
        assert!(
            c.detail
                .contains("comments and approvals need the api scope"),
            "{}",
            c.detail
        );
        assert_eq!(r.checks[&4].status, Status::Pass, "{}", r.checks[&4].detail);
        // git as oauth2, scoped to the host, URL with the nested namespace
        assert_eq!(r.checks[&7].status, Status::Pass);
        assert_eq!(
            git.calls.borrow()[0],
            (
                "https://gitlab.com/grp/sub/proj.git".to_string(),
                "oauth2".to_string(),
                "https://gitlab.com/".to_string()
            )
        );
        let text = r.render(&[TOKEN]);
        assert!(!text.contains(TOKEN));
        // read_api without read_repository: git cannot fetch
        let s = server(&["read_api"], None).await;
        let r = run_with(&s, &FakeGit::ok(), &args(None), "gitlab.com").await;
        assert_eq!(r.checks[&6].status, Status::Fail);
        assert!(r.checks[&6].detail.contains("read_repository"));
        // nothing but GETs were sent
        for req in s.received_requests().await.unwrap() {
            assert_eq!(req.method.as_str(), "GET", "{}", req.url);
        }
    }

    /// A plain-http account on a private address (the mock's own 127.0.0.1): git goes over
    /// http to exactly that origin, and check 9 says the connection is not encrypted.
    #[tokio::test]
    async fn an_http_account_uses_plain_http_git_on_its_own_origin() {
        let s = server(&["api"], None).await;
        let git = FakeGit::ok();
        let host = s.address().to_string();
        let r = run_with(&s, &git, &args(Some("grp/proj")), &host).await;
        assert_eq!(r.checks[&7].status, Status::Pass, "{}", r.checks[&7].detail);
        assert_eq!(
            git.calls.borrow()[0],
            (
                format!("http://{host}/grp/proj.git"),
                "oauth2".to_string(),
                format!("http://{host}/")
            )
        );
        let c9 = &r.checks[&9];
        assert!(c9.detail.contains("plain HTTP"), "{}", c9.detail);
        assert!(!r.render(&[TOKEN]).contains(TOKEN));
    }

    #[test]
    fn expiry_warning_within_14_days() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day);
        assert_eq!(expiry_warning(None, today()), None);
        assert_eq!(expiry_warning(d(2026, 12, 31), today()), None);
        assert_eq!(expiry_warning(d(2026, 11, 29), today()), None, "15 days");
        assert!(expiry_warning(d(2026, 11, 28), today())
            .unwrap()
            .contains("in 14 day(s)"));
        assert_eq!(
            expiry_warning(d(2026, 11, 14), today()).as_deref(),
            Some("the token expires today")
        );
        assert!(expiry_warning(d(2026, 11, 1), today())
            .unwrap()
            .contains("expired"));
    }

    #[tokio::test]
    async fn an_expiring_token_fails_check_6_with_a_hint() {
        let s = server(&["api"], Some("2026-11-20")).await;
        let r = run_with(&s, &FakeGit::ok(), &args(None), "gitlab.com").await;
        let c = &r.checks[&6];
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("expires in 6 day(s)"), "{}", c.detail);
        assert!(c.hint.as_deref().unwrap().contains("before it expires"));
    }

    #[tokio::test]
    async fn cert_untrusted_hint_shown_for_api_and_git_separately() {
        // the API answers (9 passes) while git distrusts the host's certificate (7 fails)
        let s = server(&["api"], None).await;
        let git = FakeGit {
            result: Err("fatal: unable to access 'https://git.corp.example/gitlab/grp/sub/proj.git/': SSL certificate problem: unable to get local issuer certificate".into()),
            calls: RefCell::new(Vec::new()),
        };
        let r = run_with(
            &s,
            &git,
            &args(Some("grp/sub/proj")),
            "git.corp.example/gitlab",
        )
        .await;
        assert_eq!(r.checks[&9].status, Status::Pass);
        let g = &r.checks[&7];
        assert_eq!(g.status, Status::Fail);
        assert!(g.hint.as_deref().unwrap().contains("sslBackend schannel"));
        assert!(g.hint.as_deref().unwrap().contains("check 9"));
        // relative URL root: clone URL under the prefix, header scoped to the host
        assert_eq!(
            git.calls.borrow()[0],
            (
                "https://git.corp.example/gitlab/grp/sub/proj.git".to_string(),
                "oauth2".to_string(),
                "https://git.corp.example/".to_string()
            )
        );
        // and the other way round: an API certificate failure is check 9's, with its hint
        let unreachable = GitLabBases {
            web_base: "https://127.0.0.1:9".into(),
            api_base: "https://127.0.0.1:9/api/v4".into(),
            git_host: "127.0.0.1:9".into(),
        };
        let a = args(None);
        let inp = Inputs {
            args: &a,
            bases: &unreachable,
            git: &FakeGit::ok(),
            proxy_var: None,
            today: today(),
        };
        let r = run(&inp, Ok((TOKEN.into(), Source::EnvGitLab))).await;
        assert_eq!(r.checks[&9].status, Status::Fail);
        assert_eq!(r.checks[&2].status, Status::Skip);
    }

    #[tokio::test]
    async fn a_refused_token_skips_the_rest_and_write_test_is_skipped() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
            )
            .mount(&s)
            .await;
        let mut a = args(Some("grp/proj"));
        a.pr = Some(3);
        a.write_test = true;
        let r = run_with(&s, &FakeGit::ok(), &a, "gitlab.com").await;
        assert_eq!(r.checks[&2].status, Status::Fail);
        assert!(r.checks[&2].hint.as_deref().unwrap().contains("read_api"));
        assert_eq!(r.checks[&3].status, Status::Skip);
        assert_eq!(r.write[0].status, Status::Skip);
        assert!(r.write[0].detail.contains("no GitLab write test"));
    }

    #[test]
    fn the_env_account_needs_the_token_and_a_matching_name() {
        let env = |k: &str| match k {
            ENV_GITLAB_TOKEN => Some("t".to_string()),
            ENV_GITLAB_URL => Some("https://git.corp.example/gitlab".to_string()),
            _ => None,
        };
        let a = env_account(Some("git.corp.example/gitlab"), env).unwrap();
        assert_eq!(a.api_base, "https://git.corp.example/gitlab/api/v4");
        assert_eq!(a.forge, Forge::GitLab);
        assert!(env_account(Some("gitlab"), env).is_some());
        assert!(env_account(Some("github"), env).is_none());
        let plain = |k: &str| (k == ENV_GITLAB_TOKEN).then(|| "t".to_string());
        assert_eq!(
            env_account(Some("gitlab.com"), plain).unwrap().git_host,
            "gitlab.com"
        );
        let secrets = store::MemorySecrets::new();
        assert!(load_token(&secrets, &a, |_| None)
            .unwrap_err()
            .contains(ENV_GITLAB_TOKEN));
        secrets.set(&token_key(&a.id), "stored").unwrap();
        assert_eq!(
            load_token(&secrets, &a, |_| None).unwrap(),
            ("stored".into(), Source::Stored)
        );
    }

    #[test]
    fn scope_note_needs_read_api_and_git_access() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(scope_note(&s(&["read_user"])).is_err());
        assert!(scope_note(&s(&["read_api"]))
            .unwrap_err()
            .contains("read_repository"));
        assert!(scope_note(&s(&["api"])).is_ok());
        assert!(scope_note(&s(&["read_api", "read_repository"])).is_ok());
    }
}
