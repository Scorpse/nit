use super::*;

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
    pub(super) fn label(self) -> &'static str {
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
    pub(super) email: String,
    pub(super) token: String,
    pub(super) source: Source,
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
pub(super) fn non_empty(v: Option<String>) -> Option<String> {
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
