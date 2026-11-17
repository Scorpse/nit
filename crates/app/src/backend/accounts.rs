use super::*;

/// A new, random, non-secret account id (`acc-` and 16 hex digits).
pub(super) fn new_account_id() -> AccountId {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    AccountId(format!("acc-{:016x}", h.finish()))
}

/// Stores a checked Bitbucket sign-in as a new account: credentials first (read back),
/// then the account row. Refuses a user that already has an account here. Returns the new
/// id and the user's name.
pub(super) fn add_account(
    secrets: &dyn SecretStore,
    store: &Mutex<Store>,
    email: &str,
    token: &str,
    me: &UserRef,
) -> Result<(AccountId, String), String> {
    let existing = lock_ok(store).accounts().unwrap_or_default();
    let signed_in = load_me(&lock_ok(store)).map(|m| m.id);
    let duplicate = existing
        .iter()
        .any(|a| a.forge == Forge::BitbucketCloud && a.user_id == me.id)
        || signed_in.as_deref() == Some(me.id.as_str());
    if duplicate {
        return Err(format!("{} is already added", me.display_name));
    }
    let id = new_account_id();
    let (ek, tk) = (email_key(&id), token_key(&id));
    let stored = secrets
        .set(&ek, email)
        .and_then(|_| secrets.set(&tk, token))
        .and_then(|_| Ok((secrets.get(&ek)?, secrets.get(&tk)?)));
    let verified = matches!(&stored, Ok((Some(e), Some(t))) if e == email && t == token);
    let account = Account {
        id: id.clone(),
        ..Account::default_bitbucket(me.id.clone(), me.display_name.clone())
    };
    if !verified || lock_ok(store).upsert_account(&account).is_err() {
        let _ = store::delete_account_keys(secrets, &id);
        return Err("Could not save the account".into());
    }
    Ok((id, me.display_name.clone()))
}

/// Stores a checked GitHub sign-in as a new account: the token only (read back), then the
/// account row with the derived bases. Refuses a user that already has an account on the
/// same host.
pub(super) fn add_github(
    secrets: &dyn SecretStore,
    store: &Mutex<Store>,
    bases: &GitHubBases,
    token: &str,
    me: &UserRef,
) -> Result<(AccountId, String), String> {
    let existing = lock_ok(store).accounts().unwrap_or_default();
    let duplicate = existing
        .iter()
        .any(|a| a.forge == Forge::GitHub && a.git_host == bases.git_host && a.user_id == me.id);
    let shown = me.login.clone().unwrap_or_else(|| me.display_name.clone());
    if duplicate {
        return Err(format!("{shown} on {} is already added", bases.git_host));
    }
    let id = new_account_id();
    let tk = token_key(&id);
    let verified = secrets
        .set(&tk, token)
        .and_then(|_| secrets.get(&tk))
        .is_ok_and(|t| t.as_deref() == Some(token));
    let account = Account {
        id: id.clone(),
        forge: Forge::GitHub,
        web_base: bases.web_base.clone(),
        api_base: bases.api_base.clone(),
        graphql_base: Some(bases.graphql_url.clone()),
        git_host: bases.git_host.clone(),
        user_id: me.id.clone(),
        login: me.login.clone().unwrap_or_default(),
        display_name: me.display_name.clone(),
        auth_kind: "pat".into(),
        enabled: true,
    };
    if !verified || lock_ok(store).upsert_account(&account).is_err() {
        let _ = store::delete_account_keys(secrets, &id);
        return Err("Could not save the account".into());
    }
    Ok((id, format!("{shown} ({})", bases.git_host)))
}

/// What a failed GitHub sign-in means for the user (never a URL or the token).
pub(super) fn github_signin_error(e: &ProviderError) -> String {
    match e {
        ProviderError::Auth => {
            "GitHub did not accept the token (mistyped, expired or revoked)".into()
        }
        // the SSO hint; its authorization URL is not part of the error
        ProviderError::Rejected {
            status: 403,
            message,
        } => format!("GitHub refused: {message}"),
        ProviderError::Forbidden => "The token was accepted but may not read your user".into(),
        ProviderError::RateLimited => "GitHub's rate limit is reached; try again later".into(),
        e => format!("Could not connect ({})", e.user_message()),
    }
}

/// "Test connection": who the token signs in as, its kind and how many repositories it
/// sees. Sends only reads.
pub(super) async fn github_connection_summary(p: &GitHubProvider) -> Result<String, String> {
    let me = p.me().await.map_err(|e| github_signin_error(&e))?;
    let kind = match p.client().oauth_scopes() {
        Some(s) if s.split(',').any(|x| x.trim() == "repo") => "classic token (repo scope)".into(),
        Some(s) => format!(
            "classic token without the repo scope (has: {}); private repositories stay hidden",
            if s.trim().is_empty() {
                "none"
            } else {
                s.trim()
            }
        ),
        None => "fine-grained token".to_string(),
    };
    let repos = match p.discover().await {
        Ok(d) => format!("{} repositories visible", d.repos.len()),
        Err(e) => format!("listing repositories failed: {}", e.user_message()),
    };
    let who = me.login.unwrap_or(me.display_name);
    Ok(format!("Signed in as {who}; {kind}; {repos}"))
}

/// The store meta key of an account's token expiry (a date, no secret).
pub(super) fn expiry_key(id: &AccountId) -> String {
    format!("account.{}.token_expires", id.as_str())
}

/// When `id`'s token expires, as recorded at sign-in (GitLab only).
pub fn token_expiry(store: &Store, id: &AccountId) -> Option<jiff::civil::Date> {
    store
        .get_meta(&expiry_key(id))
        .ok()
        .flatten()
        .and_then(|d| d.parse::<jiff::civil::Date>().ok())
}

/// Stores a checked GitLab sign-in as a new account: the token only (read back), then the
/// account row with the derived bases and the token's expiry date. Refuses a user that
/// already has an account on the same host.
pub(super) fn add_gitlab(
    secrets: &dyn SecretStore,
    store: &Mutex<Store>,
    bases: &GitLabBases,
    token: &str,
    me: &UserRef,
    expires: Option<jiff::civil::Date>,
) -> Result<(AccountId, String), String> {
    let existing = lock_ok(store).accounts().unwrap_or_default();
    let duplicate = existing
        .iter()
        .any(|a| a.forge == Forge::GitLab && a.git_host == bases.git_host && a.user_id == me.id);
    let shown = me.login.clone().unwrap_or_else(|| me.display_name.clone());
    if duplicate {
        return Err(format!("{shown} on {} is already added", bases.git_host));
    }
    let id = new_account_id();
    let tk = token_key(&id);
    let verified = secrets
        .set(&tk, token)
        .and_then(|_| secrets.get(&tk))
        .is_ok_and(|t| t.as_deref() == Some(token));
    let account = Account {
        id: id.clone(),
        forge: Forge::GitLab,
        web_base: bases.web_base.clone(),
        api_base: bases.api_base.clone(),
        graphql_base: None,
        git_host: bases.git_host.clone(),
        user_id: me.id.clone(),
        login: me.login.clone().unwrap_or_default(),
        display_name: me.display_name.clone(),
        auth_kind: "pat".into(),
        enabled: true,
    };
    if !verified || lock_ok(store).upsert_account(&account).is_err() {
        let _ = store::delete_account_keys(secrets, &id);
        return Err("Could not save the account".into());
    }
    if let Some(d) = expires {
        let _ = lock_ok(store).set_meta(&expiry_key(&id), &d.to_string());
    }
    Ok((id, format!("{shown} ({})", bases.git_host)))
}

/// What a failed GitLab sign-in means for the user (never a URL or the token).
pub(super) fn gitlab_signin_error(e: &ProviderError) -> String {
    match e {
        ProviderError::Auth => {
            "GitLab did not accept the token (mistyped, expired or revoked)".into()
        }
        ProviderError::Forbidden => {
            "The token was accepted but may not read your user (it needs read_api)".into()
        }
        ProviderError::RateLimited => "GitLab's rate limit is reached; try again later".into(),
        e => format!("Could not connect ({})", e.user_message()),
    }
}

/// The "Test connection" line for a GitLab token: who, its scopes, its expiry, what it sees.
pub(super) fn gitlab_token_summary(
    who: &str,
    info: &TokenInfo,
    today: jiff::civil::Date,
    repos: &str,
) -> String {
    use crate::checktoken::gitlab::{expiry_warning, scope_note};
    let scopes = match scope_note(&info.scopes) {
        Ok(note) | Err(note) => note,
    };
    let expiry = expiry_warning(info.expires_at, today)
        .map(|w| format!("warning: {w}"))
        .unwrap_or_else(|| match info.expires_at {
            Some(d) => format!("expires on {d}"),
            None => "no expiry date".into(),
        });
    format!("Signed in as {who}; {scopes}; {expiry}; {repos}")
}

/// "Test connection" for GitLab: user, token details and the projects it sees (reads only).
pub(super) async fn gitlab_connection_summary(p: &GitLabProvider) -> Result<String, String> {
    let me = p.me().await.map_err(|e| gitlab_signin_error(&e))?;
    let repos = match p.discover().await {
        Ok(d) => format!("{} projects visible", d.repos.len()),
        Err(e) => format!("listing projects failed: {}", e.user_message()),
    };
    let who = me.login.unwrap_or(me.display_name);
    let today = jiff::Zoned::now().date();
    Ok(match p.token_info().await {
        Ok(info) => gitlab_token_summary(&who, &info, today, &repos),
        Err(e) => format!(
            "Signed in as {who}; token details not readable ({}); {repos}",
            e.user_message()
        ),
    })
}

/// `Backend::remove_account`: keys first (a failure leaves everything as it was), then the
/// account's rows; the default account is refused (signing out is its removal).
pub(super) fn remove_account(
    secrets: &dyn SecretStore,
    store: &Mutex<Store>,
    reg: &ProviderRegistry,
    id: &AccountId,
) -> Result<String, String> {
    if *id == AccountId::default_bitbucket() {
        return Err("Sign out to remove the signed-in account".into());
    }
    store::delete_account_keys(secrets, id)
        .map_err(|e| format!("Could not remove the saved credentials: {e}"))?;
    lock_ok(&reg.by_account).remove(id);
    let _ = lock_ok(store).delete_meta(&expiry_key(id));
    let _ = lock_ok(store).delete_meta(&label_key(id));
    let removed = lock_ok(store)
        .remove_account(id)
        .map_err(|_| "Could not remove the account's cached data".to_string())?;
    Ok(format!(
        "Removed: {} cached pull request(s); {} draft(s) kept",
        removed.prs, removed.drafts_kept
    ))
}

/// The store meta key of an account's label (a name the user chose, no secret).
pub(super) fn label_key(id: &AccountId) -> String {
    format!("account.{}.label", id.as_str())
}

/// The name an account is shown by: the label the user gave it, else its login, else its
/// display name, else its id. Two accounts of one forge and host differ by it.
pub fn account_label(store: &Store, a: &Account) -> String {
    match store.get_meta(&label_key(&a.id)).ok().flatten() {
        Some(l) if !l.trim().is_empty() => l.trim().to_string(),
        _ => default_label(a),
    }
}

/// `account_label` of an account without a label of its own.
pub fn default_label(a: &Account) -> String {
    [&a.login, &a.display_name]
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or(a.id.as_str())
        .to_string()
}

/// Sets account `id`'s label (trimmed); an empty one goes back to the default.
pub fn set_account_label(store: &Store, id: &AccountId, label: &str) -> store::StoreResult<()> {
    match label.trim() {
        "" => store.delete_meta(&label_key(id)),
        l => store.set_meta(&label_key(id), l),
    }
}

/// Enables or disables an account: a disabled one is not synced or listed, its repository
/// selection and cache stay as they are.
pub(super) fn set_enabled(store: &Store, id: &AccountId, enabled: bool) -> Result<(), String> {
    store
        .set_account_enabled(id, enabled)
        .map_err(|_| "Could not change the account".to_string())
}

pub(super) const META_ME: &str = "me";

/// Stores only uuid and display name (no secrets).
pub(super) fn save_me(store: &Mutex<Store>, me: &UserRef) {
    let guard = lock_ok(store);
    let _ = guard.set_meta(META_ME, &format!("{}\n{}", me.id, me.display_name));
}

/// Records that `account` is signed in as `me`: when it was another user before, that
/// account's rebuildable rows go first (drafts stay, other accounts are untouched, see
/// `Store::adopt_user`). The default account's `me` is remembered for offline views.
pub(super) fn adopt_account(
    store: &Mutex<Store>,
    account: &AccountId,
    me: &UserRef,
) -> Result<(), String> {
    {
        let guard = lock_ok(store);
        guard
            .adopt_user(account, &me.id, &me.display_name)
            .map_err(|_| "Could not prepare the cache for this account".to_string())?;
    }
    if *account == AccountId::default_bitbucket() {
        save_me(store, me);
    }
    Ok(())
}

pub(super) fn load_me(store: &Store) -> Option<UserRef> {
    let raw = store.get_meta(META_ME).ok().flatten()?;
    let (uuid, name) = raw.split_once('\n')?;
    (!uuid.is_empty()).then(|| UserRef {
        id: uuid.into(),
        display_name: name.into(),
        login: None,
    })
}
