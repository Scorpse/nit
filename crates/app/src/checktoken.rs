//! `nit --check-token [--account <id|host>] [--repo <workspace/repo>] [--pr <id>]`: a
//! read-only self-test of one account's credentials. Without `--account` it tests the
//! signed-in default Bitbucket account (stored credentials, else `NIT_EMAIL` / `NIT_TOKEN`;
//! the old names `PRVIEW_EMAIL` / `PRVIEW_TOKEN` still work, deprecated); `--account` picks
//! another account by its id or its host. The checks run per forge: Bitbucket Cloud here,
//! GitHub in `github` (read-only checks; `NIT_GITHUB_TOKEN` / `NIT_GITHUB_URL` stand in for an
//! account not added yet, with `--account github` or its host), GitLab in `gitlab` (read-only
//! checks incl. token scopes and expiry; `NIT_GITLAB_TOKEN` / `NIT_GITLAB_URL` with
//! `--account gitlab` or its host). Like the app, it first moves
//! a sign-in saved under the old name `prview` to `nit` (see `brand`).
//!
//! Every check ends PASS, FAIL or SKIP with a short reason and, on a failure, a hint. The
//! report goes to the parent console (when there is one) and always to
//! `token-check.txt` in the data folder (`brand::data_dir`; Windows `%LOCALAPPDATA%\nit`).
//! The token never appears in the report: it is
//! never formatted into it, and the rendered text is scrubbed of it once more.
//!
//! `--write-test --repo <w/r> --pr <id> --i-understand-this-posts` additionally posts one
//! PR-level comment through the provider's comment path, reads it back and deletes it.

mod github;
pub mod gitlab;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use gitlocal::{GitAuth, BITBUCKET_GIT_USER as GIT_TOKEN_USER};
use prcore::comments::NewComment;
use prcore::model::{AccountId, Forge, PrState, RepoRef, BITBUCKET_HOST};
use providers::bitbucket::BitbucketProvider;
use providers::{BbClient, Provider, ProviderError, CERT_UNTRUSTED};
use serde_json::Value;
use store::{
    email_key, token_key, Account, SecretStore, CREDENTIAL_STORE, LEGACY_BB_EMAIL, LEGACY_BB_TOKEN,
};

pub const ENV_EMAIL: &str = "NIT_EMAIL";
pub const ENV_TOKEN: &str = "NIT_TOKEN";
/// Deprecated names from before the rename, still read when the new ones are unset.
pub const LEGACY_ENV_EMAIL: &str = "PRVIEW_EMAIL";
pub const LEGACY_ENV_TOKEN: &str = "PRVIEW_TOKEN";
const REPORT_FILE: &str = "token-check.txt";
/// At most this many repositories are asked for open PRs when no `--repo` is given.
const MAX_REPOS_SCANNED: usize = 25;
const WRITE_TEST_TEXT: &str = "nit token self-test: this comment was posted by \
    `nit --check-token --write-test` and is deleted again right away.";

const FLAG: &str = "--check-token";
const WRITE_FLAG: &str = "--write-test";
const CONSENT_FLAG: &str = "--i-understand-this-posts";
const ACCOUNT_FLAG: &str = "--account";

/// What `--check-token` was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckArgs {
    /// `--account`: an account id, or the host of exactly one account.
    pub account: Option<String>,
    /// `--repo`, checked for the selected account's forge by `repo_for`.
    pub repo: Option<RepoRef>,
    pub pr: Option<u64>,
    pub write_test: bool,
}

/// One workspace or repository name: what Bitbucket allows in a slug, nothing that could
/// change the meaning of a URL path.
fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '.'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn flag_value(args: &[String], name: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    match args.get(i + 1) {
        Some(v) if !v.starts_with("--") && !v.is_empty() => Ok(Some(v.clone())),
        _ => Err(format!("{name} needs a value")),
    }
}

/// The `--check-token` options of the full argv; `None` when `--check-token` is absent (its
/// options are then refused). Unknown flags are refused by `validate_args` before this runs.
pub fn parse_args(args: &[String]) -> Result<Option<CheckArgs>, String> {
    let has = |f: &str| args.iter().any(|a| a == f);
    let own = ["--repo", "--pr", ACCOUNT_FLAG, WRITE_FLAG, CONSENT_FLAG];
    if !has(FLAG) {
        return match own.iter().find(|f| has(f)) {
            Some(f) => Err(format!("{f} needs --check-token")),
            None => Ok(None),
        };
    }
    let mut rest = args.iter().skip(1);
    while let Some(a) = rest.next() {
        match a.as_str() {
            FLAG | WRITE_FLAG | CONSENT_FLAG => {}
            "--repo" | "--pr" | ACCOUNT_FLAG => {
                rest.next();
            }
            other => {
                return Err(format!(
                    "--check-token cannot be combined with {other} (it runs on its own)"
                ))
            }
        }
    }
    // nested namespaces parse here; whether the forge has them is `repo_for`'s call
    let repo = match flag_value(args, "--repo")? {
        Some(v) => Some(
            RepoRef::parse(BITBUCKET_HOST, &v)
                .filter(|r| r.namespace.split('/').all(valid_slug) && valid_slug(&r.slug))
                .ok_or_else(|| format!("--repo needs <workspace>/<repo-slug>, got {v:?}"))?,
        ),
        None => None,
    };
    let account = flag_value(args, ACCOUNT_FLAG)?;
    let pr = match flag_value(args, "--pr")? {
        Some(v) => Some(
            v.parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("--pr needs a pull request number, got {v:?}"))?,
        ),
        None => None,
    };
    if pr.is_some() && repo.is_none() {
        return Err("--pr needs --repo <workspace>/<repo-slug>".into());
    }
    let write_test = has(WRITE_FLAG);
    if write_test && (repo.is_none() || pr.is_none() || !has(CONSENT_FLAG)) {
        return Err(format!(
            "{WRITE_FLAG} posts (and deletes) a comment and needs all of --repo <workspace>/<repo> \
             --pr <id> {CONSENT_FLAG}"
        ));
    }
    if has(CONSENT_FLAG) && !write_test {
        return Err(format!("{CONSENT_FLAG} only goes with {WRITE_FLAG}"));
    }
    Ok(Some(CheckArgs {
        account,
        repo,
        pr,
        write_test,
    }))
}

/// The account to test: the one `wanted` names by id, else the only account whose host
/// (`git_host`, or the host of its web address) is `wanted`; without `--account` the
/// signed-in default account. A cache without accounts still has the default sign-in.
pub fn select_account(accounts: &[Account], wanted: Option<&str>) -> Result<Account, String> {
    let default = || {
        accounts
            .iter()
            .find(|a| a.id == AccountId::default_bitbucket())
            .cloned()
            .unwrap_or_else(|| Account::default_bitbucket(String::new(), String::new()))
    };
    let Some(w) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(default());
    };
    if let Some(a) = accounts.iter().find(|a| a.id.as_str() == w) {
        return Ok(a.clone());
    }
    if accounts.is_empty() && w == AccountId::default_bitbucket().as_str() {
        return Ok(default());
    }
    let host = w.to_ascii_lowercase();
    let on_host: Vec<&Account> = accounts
        .iter()
        .filter(|a| a.git_host == host || host_of(&a.web_base).to_ascii_lowercase() == host)
        .collect();
    let ids = |v: &[&Account]| {
        v.iter()
            .map(|a| a.id.as_str().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match on_host.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!(
            "no account {w:?}; known accounts: {}",
            ids(&accounts.iter().collect::<Vec<_>>())
        )),
        many => Err(format!(
            "{} accounts live on {w}; name one by id: {}",
            many.len(),
            ids(many)
        )),
    }
}

/// `--repo` on `account`'s forge, with the account's host: Bitbucket workspaces and GitHub
/// owners are one segment, GitLab namespaces may nest (`group/sub/project`).
pub fn repo_for(account: &Account, repo: &RepoRef) -> Result<RepoRef, String> {
    let nested = repo.namespace.contains('/');
    if nested && account.forge != Forge::GitLab {
        return Err(format!(
            "--repo {}: {} has no nested namespaces; use <workspace>/<repo>",
            repo.full_name(),
            account.forge.label()
        ));
    }
    Ok(RepoRef {
        host: account.git_host.clone(),
        ..repo.clone()
    })
}

// ---------------------------------------------------------------------------------------
// Credentials

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Stored,
    Env,
    /// `NIT_GITHUB_TOKEN` (GitHub accounts).
    EnvGitHub,
    /// `NIT_GITLAB_TOKEN` (GitLab accounts).
    EnvGitLab,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Stored => "the system credential store (saved by nit)",
            Source::Env => "the environment variables NIT_EMAIL and NIT_TOKEN (or the deprecated PRVIEW_EMAIL and PRVIEW_TOKEN)",
            Source::EnvGitHub => "the environment variable NIT_GITHUB_TOKEN",
            Source::EnvGitLab => "the environment variable NIT_GITLAB_TOKEN",
        }
    }
}

/// Email and token. `Debug` never shows either.
pub struct Creds {
    email: String,
    token: String,
    source: Source,
}

impl fmt::Debug for Creds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Creds")
            .field("email", &"***")
            .field("token", &"***")
            .field("source", &self.source)
            .finish()
    }
}

/// The value as stored (the app uses it untrimmed too), unless it is blank.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// The stored credentials, else the environment's. `Err` is the reason (no values).
pub fn load_credentials(
    secrets: &dyn SecretStore,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Creds, String> {
    // the default account's keys; the pre-account ones until the app moved them
    let (ek, tk) = crate::accounts::bitbucket_keys();
    let stored = match (secrets.get(&ek), secrets.get(&tk)) {
        (Ok(None), Ok(None)) => (secrets.get(LEGACY_BB_EMAIL), secrets.get(LEGACY_BB_TOKEN)),
        both => both,
    };
    let store_note = match stored {
        (Ok(e), Ok(t)) => match (non_empty(e), non_empty(t)) {
            (Some(email), Some(token)) => {
                return Ok(Creds {
                    email,
                    token,
                    source: Source::Stored,
                })
            }
            (Some(_), None) => format!("{CREDENTIAL_STORE} has an email but no token"),
            (None, Some(_)) => format!("{CREDENTIAL_STORE} has a token but no email"),
            (None, None) => format!("nothing saved in {CREDENTIAL_STORE}"),
        },
        _ => format!("could not read {CREDENTIAL_STORE}"),
    };
    let email = crate::brand::env_or_legacy(&env, ENV_EMAIL, LEGACY_ENV_EMAIL);
    let token = crate::brand::env_or_legacy(&env, ENV_TOKEN, LEGACY_ENV_TOKEN);
    match (email, token) {
        (Some(email), Some(token)) => Ok(Creds {
            email,
            token,
            source: Source::Env,
        }),
        (Some(_), None) => Err(format!(
            "{store_note}; {ENV_EMAIL} is set but {ENV_TOKEN} is not"
        )),
        (None, Some(_)) => Err(format!(
            "{store_note}; {ENV_TOKEN} is set but {ENV_EMAIL} is not"
        )),
        (None, None) => Err(format!(
            "{store_note}, and {ENV_EMAIL} / {ENV_TOKEN} are not set"
        )),
    }
}

/// The credentials of `account`: the default account as `load_credentials`; another
/// Bitbucket account from its own keys only (the environment names the default one). Other
/// forges are not checked yet.
pub fn load_account_credentials(
    secrets: &dyn SecretStore,
    account: &Account,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Creds, String> {
    if account.forge != Forge::BitbucketCloud {
        return Err(format!(
            "{} accounts are not checked by --check-token yet",
            account.forge.label()
        ));
    }
    if account.id == AccountId::default_bitbucket() {
        return load_credentials(secrets, env);
    }
    let email = non_empty(secrets.get(&email_key(&account.id)).ok().flatten());
    let token = non_empty(secrets.get(&token_key(&account.id)).ok().flatten());
    match (email, token) {
        (Some(email), Some(token)) => Ok(Creds {
            email,
            token,
            source: Source::Stored,
        }),
        _ => Err(format!(
            "{CREDENTIAL_STORE} has no complete sign-in for account {}",
            account.id.as_str()
        )),
    }
}

/// Every token the report must never show: the one in use, every stored account's, and the
/// environment's (also when only half of a sign-in is set).
pub fn tokens_to_scrub(
    secrets: &dyn SecretStore,
    accounts: &[Account],
    creds: &Result<Creds, String>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = creds.iter().map(|c| c.token.clone()).collect();
    let (_, default_token) = crate::accounts::bitbucket_keys();
    let keys = accounts
        .iter()
        .map(|a| token_key(&a.id))
        .chain([default_token, LEGACY_BB_TOKEN.to_string()]);
    out.extend(keys.filter_map(|k| secrets.get(&k).ok().flatten()));
    out.extend(
        [
            ENV_TOKEN,
            LEGACY_ENV_TOKEN,
            github::ENV_GITHUB_TOKEN,
            gitlab::ENV_GITLAB_TOKEN,
        ]
        .into_iter()
        .filter_map(env),
    );
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------------------
// Report

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

#[derive(Clone)]
pub struct Check {
    pub id: String,
    pub title: &'static str,
    pub status: Status,
    pub detail: String,
    pub hint: Option<String>,
}

#[derive(Default)]
pub struct Report {
    /// Which account was tested (forge, host, id; no secret).
    pub account: Option<String>,
    pub source: Option<Source>,
    /// The numbered checks 1 to 9.
    pub checks: BTreeMap<u8, Check>,
    /// `--write-test` steps, in order; empty without it.
    pub write: Vec<Check>,
    pub write_target: Option<String>,
}

fn title(n: u8) -> &'static str {
    match n {
        1 => "Credentials present",
        2 => "REST sign-in (GET /user)",
        3 => "List workspaces and repositories",
        4 => "List open pull requests",
        5 => "Read a pull request (diffstat, diff, comments)",
        6 => "Read approvals and permissions",
        7 => "Git sign-in (git ls-remote)",
        8 => "System git",
        9 => "HTTPS / TLS / proxy to the API",
        _ => "",
    }
}

impl Report {
    fn set(&mut self, n: u8, status: Status, detail: impl Into<String>, hint: Option<String>) {
        let c = Check {
            id: n.to_string(),
            title: title(n),
            status,
            detail: detail.into(),
            hint,
        };
        self.checks.insert(n, c);
    }

    fn pass(&mut self, n: u8, detail: impl Into<String>) {
        self.set(n, Status::Pass, detail, None);
    }

    fn fail(&mut self, n: u8, detail: impl Into<String>, hint: Option<String>) {
        self.set(n, Status::Fail, detail, hint);
    }

    fn skip(&mut self, n: u8, detail: impl Into<String>) {
        self.set(n, Status::Skip, detail, None);
    }

    fn step(
        &mut self,
        n: usize,
        title: &'static str,
        status: Status,
        detail: String,
        hint: Option<String>,
    ) {
        self.write.push(Check {
            id: format!("W{n}"),
            title,
            status,
            detail,
            hint,
        });
    }

    #[cfg(test)]
    fn status(&self, n: u8) -> Option<Status> {
        self.checks.get(&n).map(|c| c.status)
    }

    pub fn problems(&self) -> usize {
        self.checks
            .values()
            .chain(&self.write)
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    /// The report as text, ending with `ALL OK` or `N problems`; every occurrence of a
    /// `secret` is replaced by `***`.
    pub fn render(&self, secrets: &[&str]) -> String {
        let mut out = String::from("nit token check\n");
        if let Some(a) = &self.account {
            out.push_str(&format!("Account: {a}\n"));
        }
        if let Some(s) = self.source {
            out.push_str(&format!("Credentials from: {}\n", s.label()));
        }
        out.push('\n');
        let line = |out: &mut String, c: &Check| {
            out.push_str(&format!(
                "[{}] {}. {}: {}\n",
                c.status.tag(),
                c.id,
                c.title,
                c.detail
            ));
            if let Some(h) = &c.hint {
                out.push_str(&format!("       Fix: {h}\n"));
            }
        };
        for c in self.checks.values() {
            line(&mut out, c);
        }
        if !self.write.is_empty() {
            out.push_str(&format!(
                "\nWrite test on {}:\n",
                self.write_target.as_deref().unwrap_or("?")
            ));
            for c in &self.write {
                line(&mut out, c);
            }
        }
        out.push('\n');
        out.push_str(&match self.problems() {
            0 => "ALL OK".to_string(),
            1 => "1 problem".to_string(),
            n => format!("{n} problems"),
        });
        out.push('\n');
        redact(&out, secrets)
    }
}

/// Replaces every secret (ignoring ones too short to be a real token) by `***`.
fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| s.len() >= 4)
        .fold(text.to_string(), |acc, s| acc.replace(s, "***"))
}

// ---------------------------------------------------------------------------------------
// Hints

const HINT_CREDS: &str = "sign in once in nit (it saves the email and token in the system \
    credential store), or set NIT_EMAIL and NIT_TOKEN in this terminal and run again";
const HINT_NEW_TOKEN: &str = "check that the email is your Atlassian account email and the \
    token an Atlassian API token (Security, API tokens, \"Create API token with scopes\", \
    Bitbucket) that is not expired or revoked; create a new one and sign in again";

/// A hint for an API error; `scope` names the access the call needs.
fn api_hint(e: &ProviderError, scope: &str) -> Option<String> {
    match e {
        ProviderError::Auth => Some(HINT_NEW_TOKEN.into()),
        ProviderError::Forbidden | ProviderError::Rejected { status: 403, .. } => Some(format!(
            "the token was accepted but lacks access: it needs {scope}. Create a token with that \
             scope and sign in again (or ask for access to the repository)"
        )),
        ProviderError::NotFound | ProviderError::Rejected { status: 404, .. } => Some(
            "it does not exist or the token cannot see it: check --repo / --pr and that your \
             account has access"
                .into(),
        ),
        ProviderError::Network(m) if m == CERT_UNTRUSTED => Some(cert_hint()),
        ProviderError::Network(_) => Some("network problem, see check 9".into()),
        ProviderError::RateLimited => {
            Some("Bitbucket is rate-limiting; wait a few minutes and run again".into())
        }
        _ => None,
    }
}

fn cert_hint() -> String {
    "a TLS-inspecting proxy (company network) presents a certificate Windows does not trust: \
     ask IT to install the company root certificate in Windows (Trusted Root Certification \
     Authorities)"
        .into()
}

// ---------------------------------------------------------------------------------------
// Git side (a trait so tests run without git or a network)

pub trait GitSide {
    /// `git --version` text (and where it is), or why git cannot be used.
    fn version(&self) -> Result<String, String>;
    /// `git ls-remote <url> HEAD` with the app's git auth; `user` overrides the user name.
    /// `Err` is git's (scrubbed) error text.
    fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String>;
}

pub struct SystemGit;

impl GitSide for SystemGit {
    fn version(&self) -> Result<String, String> {
        let git = gitlocal::Git::shared().map_err(|e| e.to_string())?;
        let out = git
            .run(&std::env::temp_dir(), &["--version"], None)
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "{} ({})",
            String::from_utf8_lossy(&out).trim(),
            git.exe().display()
        ))
    }

    fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
        let git = gitlocal::Git::shared().map_err(|e| e.to_string())?;
        gitlocal::ls_remote(&git, url, auth, user).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitFailure {
    Auth,
    NoAccess,
    Cert,
    Network,
    Timeout,
    Other,
}

fn classify_git(text: &str) -> GitFailure {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if any(&["did not finish within"]) {
        GitFailure::Timeout
    } else if any(&[
        "ssl certificate",
        "certificate verify",
        "local issuer",
        "self signed",
        "self-signed",
        "certificate chain",
        "sec_e_untrusted_root",
        "revocation",
    ]) {
        GitFailure::Cert
    } else if any(&[
        "could not resolve",
        "failed to connect",
        "couldn't connect",
        "could not connect",
        "connection refused",
        "connection timed out",
        "timed out",
        "proxy",
        "connection was reset",
    ]) {
        GitFailure::Network
    } else if any(&[
        "authentication failed",
        "401",
        "invalid credentials",
        "invalid username or password",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
    ]) {
        GitFailure::Auth
    } else if any(&[
        "not found",
        "403",
        "does not exist",
        "access denied",
        "forbidden",
    ]) {
        GitFailure::NoAccess
    } else {
        GitFailure::Other
    }
}

/// First line of git's error text, shortened (it was already scrubbed of the token).
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if line.chars().count() > 160 {
        format!("{}...", line.chars().take(157).collect::<String>())
    } else {
        line
    }
}

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
struct Me {
    display_name: String,
    username: Option<String>,
    uuid: Option<String>,
}

fn me_of(v: &Value) -> Me {
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

fn host_of(base: &str) -> String {
    base.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(base)
        .to_string()
}

fn pr_path(repo: &RepoRef, id: u64) -> String {
    format!(
        "/repositories/{}/{}/pullrequests/{id}",
        repo.namespace, repo.slug
    )
}

fn percent_encode(s: &str) -> String {
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
struct Found {
    /// The given repository, or the first one with open PRs.
    repo: Option<RepoRef>,
    /// Its first open PR.
    first_pr: Option<u64>,
    /// Any repository seen (for the git check when none has open PRs).
    any_repo: Option<RepoRef>,
}

fn check_reach(
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

fn check_git_version(r: &mut Report, git: &dyn GitSide) -> bool {
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

const SCOPE_READ_REPO: &str = "Bitbucket read access to workspaces and repositories \
    (workspace: read, repository: read)";
const SCOPE_READ_PR: &str = "Bitbucket read pull request access (pull request: read)";

/// Check 3; returns the visible workspaces.
async fn check_workspaces(
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
async fn check_open_prs(
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

async fn check_read_pr(
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

async fn check_approvals(
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

fn check_git_auth(
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

async fn write_test(
    r: &mut Report,
    provider: &BitbucketProvider,
    client: &BbClient,
    repo: &RepoRef,
    id: u64,
) {
    r.write_target = Some(format!("{} PR #{id}", repo.full_name()));
    let skip = |r: &mut Report, from: usize| {
        let steps = [
            (2, "Read the comment back"),
            (3, "Delete the comment"),
            (4, "Check it is gone"),
        ];
        for (n, t) in steps.into_iter().filter(|(n, _)| *n >= from) {
            r.step(
                n,
                t,
                Status::Skip,
                "not attempted (an earlier step failed)".into(),
                None,
            );
        }
    };
    let new = NewComment::Pr {
        text: WRITE_TEST_TEXT.into(),
    };
    // Bitbucket anchors to the current diff: no revision is pinned
    let at = providers::provider::PostRevision::default();
    let posted = match provider.post_comment(repo, id, &at, &new).await {
        Ok(c) => {
            r.step(
                1,
                "Post a PR comment",
                Status::Pass,
                format!("posted comment #{}", c.id),
                None,
            );
            c
        }
        Err(e) => {
            let hint = match &e {
                ProviderError::Unconfirmed | ProviderError::PostedUnreadable => Some(
                    "the comment may exist: look at the PR in Bitbucket and delete the test \
                     comment by hand"
                        .into(),
                ),
                e => api_hint(
                    e,
                    "Bitbucket write pull request access (pull request: write)",
                ),
            };
            r.step(
                1,
                "Post a PR comment",
                Status::Fail,
                format!("could not post: {}", e.user_message()),
                hint,
            );
            return skip(r, 2);
        }
    };
    match provider.comments(repo, id).await {
        Ok(list) => match list.comments.iter().find(|c| c.id == posted.id) {
            Some(c) if c.body.trim() == WRITE_TEST_TEXT => r.step(
                2,
                "Read the comment back",
                Status::Pass,
                format!("comment #{} is listed with the posted text", c.id),
                None,
            ),
            Some(c) => r.step(
                2,
                "Read the comment back",
                Status::Fail,
                format!("comment #{} is listed but its text differs", c.id),
                None,
            ),
            None => r.step(
                2,
                "Read the comment back",
                Status::Fail,
                format!("comment #{} is not in the PR's comments", posted.id),
                None,
            ),
        },
        Err(e) => r.step(
            2,
            "Read the comment back",
            Status::Fail,
            format!("could not read the comments: {}", e.user_message()),
            api_hint(&e, SCOPE_READ_PR),
        ),
    }
    let path = format!("{}/comments/{}", pr_path(repo, id), posted.id);
    if let Err(e) = client.delete_checked(&path).await {
        r.step(
            3,
            "Delete the comment",
            Status::Fail,
            format!("could not delete: {}", e.user_message()),
            Some(format!(
                "delete the test comment #{} on {} PR #{id} by hand in Bitbucket",
                posted.id,
                repo.full_name()
            )),
        );
        return skip(r, 4);
    }
    r.step(
        3,
        "Delete the comment",
        Status::Pass,
        format!("deleted comment #{}", posted.id),
        None,
    );
    match provider.comments(repo, id).await {
        Ok(list) => {
            let left = list
                .comments
                .iter()
                .any(|c| c.id == posted.id && !c.deleted);
            if left {
                r.step(
                    4,
                    "Check it is gone",
                    Status::Fail,
                    format!("comment #{} is still listed", posted.id),
                    Some(format!(
                        "delete the test comment #{} by hand in Bitbucket",
                        posted.id
                    )),
                );
            } else {
                r.step(
                    4,
                    "Check it is gone",
                    Status::Pass,
                    "no longer listed (or marked deleted)".into(),
                    None,
                );
            }
        }
        Err(e) => r.step(
            4,
            "Check it is gone",
            Status::Fail,
            format!("could not read the comments: {}", e.user_message()),
            None,
        ),
    }
}

// ---------------------------------------------------------------------------------------
// Entry point: console, report file, exit code

fn report_path() -> PathBuf {
    crate::brand::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(REPORT_FILE)
}

fn proxy_var() -> Option<&'static str> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .into_iter()
    .find(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Whether the token may go to `api_base`: https (`Ok(None)`), plain http on a private
/// address (`Ok` with the cleartext warning), or not at all (`Err`, nothing is sent).
fn transport_note(api_base: &str) -> Result<Option<&'static str>, &'static str> {
    if !prcore::origin::transport_allowed(api_base) {
        return Err(
            "the account's API address is not https (plain http only on a private \
                    address); nothing was sent",
        );
    }
    Ok(prcore::origin::is_private_http_url(api_base).then_some(prcore::origin::CLEARTEXT_WARNING))
}

/// Runs the check and returns the process exit code (0: no problem, 1: at least one).
pub fn main(args: CheckArgs) -> i32 {
    let mut out = console::attach();
    let secrets = store::CredentialManagerSecrets::new(crate::brand::NAME);
    let _ = crate::brand::migrate_credentials(
        &secrets,
        &store::CredentialManagerSecrets::new(crate::brand::LEGACY_NAME),
    );
    let env = |k: &str| std::env::var(k).ok();
    // the accounts (no secrets) from the cache, when there is one
    let accounts = crate::brand::data_dir()
        .map(|d| d.join("cache.sqlite"))
        .filter(|p| p.exists())
        .and_then(|p| store::Store::open(&p).ok())
        .and_then(|s| s.accounts().ok())
        .unwrap_or_default();
    let account = match select_account(&accounts, args.account.as_deref()) {
        Ok(a) => a,
        // a first run without a stored GitHub or GitLab account: NIT_GITHUB_TOKEN /
        // NIT_GITHUB_URL, NIT_GITLAB_TOKEN / NIT_GITLAB_URL
        Err(e) => match github::env_account(args.account.as_deref(), env)
            .or_else(|| gitlab::env_account(args.account.as_deref(), env))
        {
            Some(a) => a,
            None => {
                out.write(&format!("--account: {e}\n"));
                return 2;
            }
        },
    };
    let args = match args
        .repo
        .as_ref()
        .map(|r| repo_for(&account, r))
        .transpose()
    {
        Ok(repo) => CheckArgs { repo, ..args },
        Err(e) => {
            out.write(&format!("{e}\n"));
            return 2;
        }
    };
    match transport_note(&account.api_base) {
        Ok(None) => {}
        Ok(Some(warning)) => out.write(&format!("warning: {warning}\n")),
        Err(e) => {
            out.write(&format!("{e}\n"));
            return 2;
        }
    }
    let github_token =
        (account.forge == Forge::GitHub).then(|| github::load_token(&secrets, &account, env));
    let gitlab_token =
        (account.forge == Forge::GitLab).then(|| gitlab::load_token(&secrets, &account, env));
    let creds = load_account_credentials(&secrets, &account, env);
    let mut secret_values = tokens_to_scrub(&secrets, &accounts, &creds, env);
    for (t, _) in [&github_token, &gitlab_token]
        .into_iter()
        .flatten()
        .flatten()
    {
        secret_values.push(t.clone());
    }
    let report = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => {
            let mut report = match (github_token, gitlab_token) {
                (_, Some(token)) => {
                    let bases = gitlab::bases_of(&account);
                    let inputs = gitlab::Inputs {
                        args: &args,
                        bases: &bases,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                        today: chrono::Local::now().date_naive(),
                    };
                    rt.block_on(gitlab::run(&inputs, token))
                }
                (Some(token), None) => {
                    let bases = github::bases_of(&account);
                    let inputs = github::Inputs {
                        args: &args,
                        bases: &bases,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                    };
                    rt.block_on(github::run(&inputs, token))
                }
                (None, None) => {
                    let inputs = Inputs {
                        args: &args,
                        api_base: &account.api_base,
                        git: &SystemGit,
                        proxy_var: proxy_var(),
                    };
                    rt.block_on(run(&inputs, creds))
                }
            };
            report.account = Some(format!(
                "{} {} ({})",
                account.forge.label(),
                account.git_host,
                account.id.as_str()
            ));
            report
        }
        Err(_) => {
            out.write("could not start the async runtime\n");
            return 1;
        }
    };
    let secrets: Vec<&str> = secret_values.iter().map(String::as_str).collect();
    let text = report.render(&secrets);
    let path = report_path();
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, &text));
    let where_line = match &saved {
        Ok(()) => format!("Report saved to {}\n", path.display()),
        Err(e) => format!("Could not save the report to {}: {e}\n", path.display()),
    };
    out.write(&format!("\n{where_line}\n{text}"));
    // Windows: started without a console (double-click, shortcut): show the file. Elsewhere
    // the report always goes to stdout (`console::attach`), so there is nothing to open.
    #[cfg(windows)]
    if !out.visible() && saved.is_ok() {
        let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
    }
    i32::from(report.problems() > 0)
}

/// Where the report is printed: the inherited stdout, the parent's console, or nowhere.
mod console {
    use std::io::Write;

    // only Windows has a console to attach to (or none)
    #[cfg_attr(not(windows), allow(dead_code))]
    pub enum Out {
        Std,
        Console(std::fs::File),
        None,
    }

    impl Out {
        pub fn write(&mut self, text: &str) {
            let _ = match self {
                Out::Std => {
                    let mut s = std::io::stdout();
                    s.write_all(text.as_bytes()).and_then(|_| s.flush())
                }
                Out::Console(f) => f.write_all(text.as_bytes()),
                Out::None => Ok(()),
            };
        }

        #[cfg_attr(not(windows), allow(dead_code))]
        pub fn visible(&self) -> bool {
            !matches!(self, Out::None)
        }
    }

    /// A GUI-subsystem exe has no console: use stdout when it was handed one (a pipe, a
    /// file, a debug build's console), else attach to the console of the parent (the
    /// terminal it was started from).
    #[cfg(windows)]
    pub fn attach() -> Out {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
        };
        // SAFETY: plain Win32 calls without pointers
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                return Out::Std;
            }
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return Out::None;
            }
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open("CONOUT$")
            .map(Out::Console)
            .unwrap_or(Out::None)
    }

    /// macOS and Unix: a program always has its standard output (a terminal, a pipe, or
    /// /dev/null under a launcher), so the report is printed there; no console to attach.
    #[cfg(not(windows))]
    pub fn attach() -> Out {
        Out::Std
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use store::MemorySecrets;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "ATATT-very-secret-token-0123456789";
    const EMAIL: &str = "ana@example.com";

    fn argv(a: &[&str]) -> Vec<String> {
        std::iter::once("nit")
            .chain(a.iter().copied())
            .map(String::from)
            .collect()
    }

    fn creds() -> Result<Creds, String> {
        Ok(Creds {
            email: EMAIL.into(),
            token: TOKEN.into(),
            source: Source::Env,
        })
    }

    /// A git that answers from a table: `ok_users` are the user names ls-remote accepts
    /// (`None` is the app's own).
    struct FakeGit {
        version: Result<String, String>,
        ok_users: Vec<Option<String>>,
        error: String,
        calls: RefCell<Vec<Option<String>>>,
    }

    impl FakeGit {
        fn ok() -> FakeGit {
            FakeGit {
                version: Ok("git version 2.47.1.windows.1".into()),
                ok_users: vec![None],
                error: String::new(),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitSide for FakeGit {
        fn version(&self) -> Result<String, String> {
            self.version.clone()
        }
        fn ls_remote(&self, _url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
            assert_eq!(auth.token, TOKEN);
            let user = user.map(String::from);
            self.calls.borrow_mut().push(user.clone());
            if self.ok_users.contains(&user) {
                Ok(())
            } else {
                Err(self.error.clone())
            }
        }
    }

    fn args(repo: Option<&str>, pr: Option<u64>, write_test: bool) -> CheckArgs {
        CheckArgs {
            account: None,
            repo: repo.and_then(RepoRef::parse_bitbucket),
            pr,
            write_test,
        }
    }

    async fn get(s: &MockServer, p: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(s)
            .await;
    }

    fn pr_json(id: u64) -> Value {
        json!({
            "id": id, "title": "Add things", "state": "OPEN",
            "author": {"uuid": "{bo}", "display_name": "Bo"},
            "source": {"branch": {"name": "feature"}, "commit": {"hash": "abc"},
                       "repository": {"full_name": "acme/api"}},
            "destination": {"branch": {"name": "main"}, "commit": {"hash": "def"},
                            "repository": {"full_name": "acme/api"}},
            "participants": [
                {"user": {"uuid": "{ana}", "display_name": "Ana"}, "role": "REVIEWER",
                 "approved": true, "state": "approved"},
                {"user": {"uuid": "{cy}", "display_name": "Cy"}, "role": "REVIEWER",
                 "approved": false, "state": null}
            ],
            "created_on": "2026-01-01T00:00:00Z", "updated_on": "2026-01-02T00:00:00Z"
        })
    }

    fn comment_json(id: u64, text: &str, bot: bool) -> Value {
        let user = if bot {
            json!({"type": "app_user", "display_name": "Sonar", "uuid": "{app}"})
        } else {
            json!({"type": "user", "display_name": "Bo", "uuid": "{bo}"})
        };
        json!({"id": id, "user": user, "content": {"raw": text},
               "created_on": "2026-01-01T00:00:00Z", "updated_on": "2026-01-01T00:00:00Z"})
    }

    /// A healthy account: one workspace `acme`, repos `empty` (no PRs) and `api` (PR 7).
    async fn healthy(display_name: &str) -> MockServer {
        let s = MockServer::start().await;
        get(
            &s,
            "/user",
            json!({"uuid": "{ana}", "display_name": display_name, "username": "ana"}),
        )
        .await;
        get(
            &s,
            "/user/workspaces",
            json!({"values": [{"workspace": {"slug": "acme"}}]}),
        )
        .await;
        get(
            &s,
            "/repositories/acme",
            json!({"values": [{"full_name": "acme/empty"}, {"full_name": "acme/api"}]}),
        )
        .await;
        get(
            &s,
            "/repositories/acme/empty/pullrequests",
            json!({"values": []}),
        )
        .await;
        get(
            &s,
            "/repositories/acme/api/pullrequests",
            json!({"values": [pr_json(7)]}),
        )
        .await;
        get(&s, "/repositories/acme/api/pullrequests/7", pr_json(7)).await;
        get(
            &s,
            "/repositories/acme/api/pullrequests/7/diffstat",
            json!({"values": [{"lines_added": 3, "lines_removed": 1}]}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repositories/acme/api/pullrequests/7/diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string("diff --git a/x b/x\n"))
            .mount(&s)
            .await;
        get(
            &s,
            "/repositories/acme/api/pullrequests/7/tasks",
            json!({"values": []}),
        )
        .await;
        get(
            &s,
            "/user/permissions/repositories",
            json!({"values": [{"permission": "write"}]}),
        )
        .await;
        s
    }

    async fn mount_comments(s: &MockServer, values: Value) {
        get(s, "/repositories/acme/api/pullrequests/7/comments", values).await;
    }

    async fn check(s: &MockServer, a: &CheckArgs, git: &FakeGit) -> Report {
        let base = s.uri();
        let inputs = Inputs {
            args: a,
            api_base: &base,
            git,
            proxy_var: None,
        };
        run(&inputs, creds()).await
    }

    fn statuses(r: &Report) -> Vec<(u8, Status)> {
        r.checks.iter().map(|(n, c)| (*n, c.status)).collect()
    }

    #[test]
    fn check_token_flags_parse_and_refuse_bad_combinations() {
        assert_eq!(parse_args(&argv(&[])).unwrap(), None);
        assert_eq!(
            parse_args(&argv(&["--check-token"])).unwrap(),
            Some(args(None, None, false))
        );
        let a = parse_args(&argv(&["--check-token", "--repo", "acme/api", "--pr", "7"])).unwrap();
        assert_eq!(a, Some(args(Some("acme/api"), Some(7), false)));
        let w = parse_args(&argv(&[
            "--check-token",
            "--write-test",
            "--pr",
            "7",
            "--repo",
            "acme/api",
            "--i-understand-this-posts",
        ]))
        .unwrap();
        assert!(w.unwrap().write_test);
        for bad in [
            vec!["--repo", "acme/api"],
            vec!["--pr", "7"],
            vec!["--write-test"],
            vec!["--check-token", "--pr", "7"],
            vec!["--check-token", "--repo"],
            vec!["--check-token", "--repo", "acme"],
            vec!["--check-token", "--repo", "acme/../x"],
            vec!["--check-token", "--repo", "acme/a?b"],
            vec!["--check-token", "--repo", "acme/api", "--pr", "x"],
            vec!["--check-token", "--repo", "acme/api", "--pr", "0"],
            vec!["--check-token", "--demo"],
            vec![
                "--check-token",
                "--write-test",
                "--repo",
                "acme/api",
                "--pr",
                "7",
            ],
            vec![
                "--check-token",
                "--write-test",
                "--repo",
                "acme/api",
                "--i-understand-this-posts",
            ],
            vec!["--check-token", "--i-understand-this-posts"],
        ] {
            assert!(parse_args(&argv(&bad)).is_err(), "{bad:?}");
        }
    }

    fn account(id: &str, forge: Forge, host: &str) -> Account {
        Account {
            id: AccountId(id.into()),
            forge,
            web_base: format!("https://{host}"),
            git_host: host.into(),
            ..Account::default_bitbucket(String::new(), String::new())
        }
    }

    #[test]
    fn transport_rule_allows_https_and_warns_on_private_http() {
        assert_eq!(transport_note("https://gitlab.com/api/v4"), Ok(None));
        assert_eq!(
            transport_note("http://192.168.1.20:8080/api/v4"),
            Ok(Some(prcore::origin::CLEARTEXT_WARNING))
        );
        for bad in [
            "http://gitlab.com/api/v4",
            "http://8.8.8.8/api/v4",
            "http://192.168.1.1.evil.com/api/v4",
            "ftp://x",
        ] {
            assert!(transport_note(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn account_flag_selects_by_id_or_host() {
        let accounts = vec![
            Account::default_bitbucket("{ana}".into(), "Ana".into()),
            account("acc-2", Forge::BitbucketCloud, "bitbucket.org"),
            account("acc-gl", Forge::GitLab, "git.corp.example/gitlab"),
        ];
        let id = |r: Result<Account, String>| r.map(|a| a.id.as_str().to_string());
        assert_eq!(
            id(select_account(&accounts, None)),
            Ok("acc-bitbucket-default".into())
        );
        assert_eq!(
            id(select_account(&accounts, Some("acc-2"))),
            Ok("acc-2".into())
        );
        assert_eq!(
            id(select_account(&accounts, Some("GIT.corp.example/gitlab"))),
            Ok("acc-gl".into())
        );
        let two = select_account(&accounts, Some("bitbucket.org")).unwrap_err();
        assert!(
            two.contains("acc-2") && two.contains("acc-bitbucket-default"),
            "{two}"
        );
        assert!(select_account(&accounts, Some("github.com")).is_err());
        // no cache yet: the default sign-in
        assert_eq!(
            id(select_account(&[], None)),
            Ok("acc-bitbucket-default".into())
        );
        let parsed = parse_args(&argv(&["--check-token", "--account", "acc-2"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.account.as_deref(), Some("acc-2"));
        assert!(parse_args(&argv(&["--account", "acc-2"])).is_err());
        assert!(parse_args(&argv(&["--check-token", "--account"])).is_err());
    }

    #[test]
    fn nested_repo_rejected_for_bitbucket_accepted_for_gitlab() {
        let a = parse_args(&argv(&["--check-token", "--repo", "group/sub/proj"]))
            .unwrap()
            .unwrap();
        let nested = a.repo.unwrap();
        let bb = account("acc-2", Forge::BitbucketCloud, "bitbucket.org");
        let gl = account("acc-gl", Forge::GitLab, "git.corp.example/gitlab");
        assert!(repo_for(&bb, &nested).unwrap_err().contains("nested"));
        let r = repo_for(&gl, &nested).unwrap();
        assert_eq!(
            (r.host.as_str(), r.namespace.as_str(), r.slug.as_str()),
            ("git.corp.example/gitlab", "group/sub", "proj")
        );
        let flat = RepoRef::parse_bitbucket("acme/api").unwrap();
        assert_eq!(repo_for(&bb, &flat).unwrap(), flat);
        assert!(parse_args(&argv(&["--check-token", "--repo", "group/../proj"])).is_err());
    }

    #[test]
    fn report_scrubs_every_account_token() {
        let s = MemorySecrets::new();
        let (two, three) = (
            account("acc-2", Forge::BitbucketCloud, "bitbucket.org"),
            account("acc-3", Forge::BitbucketCloud, "bitbucket.org"),
        );
        s.set(&email_key(&two.id), "bo@example.com").unwrap();
        s.set(&token_key(&two.id), "token-of-account-two").unwrap();
        s.set(&token_key(&three.id), "token-of-account-three")
            .unwrap();
        let creds = load_account_credentials(&s, &two, |_| None);
        assert_eq!(creds.as_ref().unwrap().source, Source::Stored);
        // a half stored account is no sign-in, and other forges are not checked yet
        assert!(load_account_credentials(&s, &three, |_| None).is_err());
        let gh = account("acc-gh", Forge::GitHub, "github.com");
        assert!(load_account_credentials(&s, &gh, |_| None)
            .unwrap_err()
            .contains("not checked"));
        let env = |k: &str| (k == ENV_TOKEN).then(|| "token-from-the-env".to_string());
        let scrub = tokens_to_scrub(&s, &[two, three], &creds, env);
        let mut r = Report {
            account: Some("Bitbucket Cloud bitbucket.org (acc-2)".into()),
            ..Report::default()
        };
        r.pass(
            2,
            "token-of-account-two token-of-account-three token-from-the-env",
        );
        let refs: Vec<&str> = scrub.iter().map(String::as_str).collect();
        let text = r.render(&refs);
        assert!(text.contains("Account: Bitbucket Cloud bitbucket.org (acc-2)"));
        for t in [
            "token-of-account-two",
            "token-of-account-three",
            "token-from-the-env",
        ] {
            assert!(!text.contains(t), "{text}");
        }
    }

    #[test]
    fn stored_credentials_win_and_the_environment_is_the_fallback() {
        let s = MemorySecrets::new();
        let env = |k: &str| match k {
            ENV_EMAIL => Some("env@example.com".to_string()),
            ENV_TOKEN => Some("env-token-value".to_string()),
            _ => None,
        };
        assert_eq!(load_credentials(&s, env).unwrap().source, Source::Env);
        let (ek, tk) = crate::accounts::bitbucket_keys();
        s.set(&ek, EMAIL).unwrap();
        s.set(&tk, TOKEN).unwrap();
        let c = load_credentials(&s, env).unwrap();
        assert_eq!((c.source, c.email.as_str()), (Source::Stored, EMAIL));
        assert!(!format!("{c:?}").contains(TOKEN));
        let none = load_credentials(&MemorySecrets::new(), |_| None).unwrap_err();
        assert!(none.contains("NIT_EMAIL"), "{none}");
        let half = load_credentials(&MemorySecrets::new(), |k| {
            (k == ENV_EMAIL).then(|| "x@example.com".to_string())
        })
        .unwrap_err();
        assert!(half.contains("NIT_TOKEN is not"), "{half}");
    }

    #[test]
    fn the_deprecated_prview_variables_still_work_and_the_new_ones_win() {
        let old = |k: &str| match k {
            LEGACY_ENV_EMAIL => Some("old@example.com".to_string()),
            LEGACY_ENV_TOKEN => Some("old-token-value".to_string()),
            _ => None,
        };
        let c = load_credentials(&MemorySecrets::new(), old).unwrap();
        assert_eq!(
            (c.source, c.email.as_str()),
            (Source::Env, "old@example.com")
        );
        let both = |k: &str| match k {
            ENV_EMAIL => Some("new@example.com".to_string()),
            ENV_TOKEN => Some("new-token-value".to_string()),
            _ => old(k),
        };
        let c = load_credentials(&MemorySecrets::new(), both).unwrap();
        assert_eq!(
            (c.email.as_str(), c.token.as_str()),
            ("new@example.com", "new-token-value")
        );
    }

    #[tokio::test]
    async fn a_healthy_account_is_all_ok_and_nothing_is_posted() {
        let s = healthy("Ana Souza").await;
        mount_comments(
            &s,
            json!({"values": [comment_json(1, "looks good", false), comment_json(2, "Quality gate passed", true)]}),
        )
        .await;
        for m in ["POST", "DELETE", "PUT"] {
            Mock::given(method(m))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&s)
                .await;
        }
        let git = FakeGit::ok();
        let r = check(&s, &args(None, None, false), &git).await;
        let text = r.render(&[TOKEN]);
        assert!(
            statuses(&r).iter().all(|(_, st)| *st == Status::Pass),
            "{text}"
        );
        assert!(text.trim_end().ends_with("ALL OK"), "{text}");
        assert!(text.contains("signed in as Ana Souza"), "{text}");
        assert!(text.contains("picked acme/api"), "{text}");
        assert!(
            text.contains("2 comment(s), 1 of them from bots / apps"),
            "{text}"
        );
        assert!(
            text.contains("1 approved") && text.contains("write"),
            "{text}"
        );
        assert!(text.contains(GIT_TOKEN_USER), "{text}");
        // only the app's own user name was tried
        assert_eq!(*git.calls.borrow(), vec![None]);
        assert!(
            !text.contains("ana@example.com") && !text.contains("{ana}"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_token_never_reaches_the_report() {
        // a hostile server and git echo the token back in every place that is shown
        let s = healthy(&format!("Ana {TOKEN}")).await;
        mount_comments(&s, json!({"values": [comment_json(1, TOKEN, false)]})).await;
        let git = FakeGit {
            version: Ok(format!("git version 2.47 {TOKEN}")),
            ok_users: vec![],
            error: format!("fatal: Authentication failed for {TOKEN}"),
            calls: RefCell::new(Vec::new()),
        };
        let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
        let text = r.render(&[TOKEN]);
        assert!(!text.contains(TOKEN), "{text}");
        assert!(text.contains("***"), "{text}");
        // no "ALL OK" with a failed git check
        assert!(text.trim_end().ends_with("1 problem"), "{text}");
    }

    #[tokio::test]
    async fn a_refused_token_fails_check_2_and_skips_what_depends_on_it() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let git = FakeGit {
            ok_users: vec![],
            error: "remote: Invalid credentials\nfatal: Authentication failed".into(),
            ..FakeGit::ok()
        };
        let r = check(&s, &args(Some("acme/api"), None, false), &git).await;
        assert_eq!(r.status(1), Some(Status::Pass));
        assert_eq!(r.status(2), Some(Status::Fail));
        for n in [3, 4, 5, 6] {
            assert_eq!(r.status(n), Some(Status::Skip), "{n}");
        }
        assert_eq!(r.status(9), Some(Status::Pass), "the server answered");
        assert_eq!(r.status(7), Some(Status::Fail));
        let text = r.render(&[TOKEN]);
        assert!(
            text.contains("HTTP 401") && text.contains("API token"),
            "{text}"
        );
        assert!(text.trim_end().ends_with("2 problems"), "{text}");
    }

    #[tokio::test]
    async fn git_reports_which_user_name_works() {
        let s = healthy("Ana").await;
        mount_comments(&s, json!({"values": []})).await;
        let git = FakeGit {
            ok_users: vec![Some(EMAIL.into())],
            error: "fatal: Authentication failed for 'https://bitbucket.org/acme/api.git/'".into(),
            ..FakeGit::ok()
        };
        let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
        let c = &r.checks[&7];
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail.contains("your account email: accepted"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("your Bitbucket username: refused"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("your account UUID: refused"),
            "{}",
            c.detail
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("only with your account email"), "{hint}");
        assert_eq!(
            *git.calls.borrow(),
            vec![
                None,
                Some(EMAIL.into()),
                Some("ana".into()),
                Some("{ana}".into())
            ]
        );
    }

    #[tokio::test]
    async fn git_network_and_certificate_failures_try_no_other_user_name() {
        let s = healthy("Ana").await;
        mount_comments(&s, json!({"values": []})).await;
        for (err, word) in [
            ("fatal: unable to access: SSL certificate problem: unable to get local issuer certificate", "certificate"),
            ("fatal: unable to access: Could not resolve host: bitbucket.org", "reach"),
        ] {
            let git = FakeGit {
                ok_users: vec![],
                error: err.into(),
                ..FakeGit::ok()
            };
            let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
            let c = &r.checks[&7];
            assert!(c.status == Status::Fail && c.detail.contains(word), "{}", c.detail);
            assert_eq!(*git.calls.borrow(), vec![None]);
        }
    }

    #[tokio::test]
    async fn an_unreachable_api_fails_check_9_not_check_2() {
        let inputs_args = args(None, None, false);
        let git = FakeGit::ok();
        let inputs = Inputs {
            args: &inputs_args,
            api_base: "http://127.0.0.1:1",
            git: &git,
            proxy_var: Some("HTTPS_PROXY"),
        };
        let r = run(&inputs, creds()).await;
        assert_eq!(r.status(9), Some(Status::Fail));
        assert_eq!(r.status(2), Some(Status::Skip));
        let text = r.render(&[TOKEN]);
        assert!(text.contains("could not reach 127.0.0.1:1"), "{text}");
        assert!(text.contains("HTTPS_PROXY"), "{text}");
    }

    #[test]
    fn a_certificate_error_is_told_apart_from_the_network() {
        let mut r = Report::default();
        let cert = Err(ProviderError::Network(CERT_UNTRUSTED.into()));
        check_reach(
            &mut r,
            &cert,
            "https://api.bitbucket.org",
            "api.bitbucket.org",
            None,
        );
        let c = &r.checks[&9];
        assert!(c.detail.contains("certificate") && c.hint.as_deref().unwrap().contains("root"));
        let net = Err(ProviderError::Network("connection failed".into()));
        check_reach(
            &mut r,
            &net,
            "https://api.bitbucket.org",
            "api.bitbucket.org",
            None,
        );
        assert!(r.checks[&9].detail.contains("could not reach"));
        check_reach(
            &mut r,
            &Err(ProviderError::Auth),
            "https://api.bitbucket.org",
            "api.bitbucket.org",
            None,
        );
        assert_eq!(r.status(9), Some(Status::Pass));
    }

    #[tokio::test]
    async fn missing_credentials_fail_check_1_and_still_check_git() {
        let a = args(None, None, false);
        let git = FakeGit::ok();
        let inputs = Inputs {
            args: &a,
            api_base: "http://127.0.0.1:1",
            git: &git,
            proxy_var: None,
        };
        let r = run(&inputs, Err("nothing saved".into())).await;
        assert_eq!(r.status(1), Some(Status::Fail));
        assert_eq!(r.status(8), Some(Status::Pass));
        assert_eq!(r.status(2), Some(Status::Skip));
        assert!(r.render(&[]).trim_end().ends_with("1 problem"));
    }

    #[tokio::test]
    async fn the_write_test_posts_reads_back_and_deletes() {
        let s = healthy("Ana").await;
        let mine = comment_json(55, WRITE_TEST_TEXT, false);
        // listed once (read back), then gone (after the delete)
        Mock::given(method("GET"))
            .and(path("/repositories/acme/api/pullrequests/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [mine]})))
            .up_to_n_times(2)
            .mount(&s)
            .await;
        mount_comments(&s, json!({"values": []})).await;
        Mock::given(method("POST"))
            .and(path("/repositories/acme/api/pullrequests/7/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(mine.clone()))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/repositories/acme/api/pullrequests/7/comments/55"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&s)
            .await;
        let r = check(&s, &args(Some("acme/api"), Some(7), true), &FakeGit::ok()).await;
        let text = r.render(&[TOKEN]);
        assert_eq!(r.write.len(), 4, "{text}");
        assert!(r.write.iter().all(|c| c.status == Status::Pass), "{text}");
        assert!(text.contains("Write test on acme/api PR #7"), "{text}");
        assert!(text.trim_end().ends_with("ALL OK"), "{text}");
    }

    #[tokio::test]
    async fn a_refused_post_is_reported_with_the_write_scope_and_nothing_is_deleted() {
        let s = healthy("Ana").await;
        mount_comments(&s, json!({"values": []})).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&s)
            .await;
        let r = check(&s, &args(Some("acme/api"), Some(7), true), &FakeGit::ok()).await;
        assert_eq!(r.write[0].status, Status::Fail);
        assert!(r.write[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("pull request: write"));
        assert!(r.write[1..].iter().all(|c| c.status == Status::Skip));
        assert_eq!(r.problems(), 1);
    }

    #[test]
    fn git_errors_are_classified() {
        assert_eq!(
            classify_git("fatal: Authentication failed for 'x'"),
            GitFailure::Auth
        );
        assert_eq!(
            classify_git("remote: Repository not found\nfatal: repository 'x' not found"),
            GitFailure::NoAccess
        );
        assert_eq!(
            classify_git("SSL certificate problem: self-signed certificate in certificate chain"),
            GitFailure::Cert
        );
        assert_eq!(
            classify_git("Failed to connect to bitbucket.org port 443"),
            GitFailure::Network
        );
        assert_eq!(
            classify_git("git did not finish within 60 seconds and was stopped"),
            GitFailure::Timeout
        );
    }
}
