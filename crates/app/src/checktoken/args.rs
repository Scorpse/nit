use super::*;

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
pub(super) fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '.'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub(super) fn flag_value(args: &[String], name: &str) -> Result<Option<String>, String> {
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
