//! Real-mode wiring: secrets, store file, background tokio runtime and sync.
//!
//! The UI thread never blocks here: `connect` spawns work on the runtime and results
//! come back as `Msg` values through an unbounded channel that the UI drains per frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gitlocal::GitAuth;
use prcore::model::{AccountId, DiffStat, Forge, PullRequest, RepoRef, UserRef};
use providers::bitbucket::BitbucketProvider;
use providers::github::{GitHubBases, GitHubProvider};
use providers::gitlab::{GitLabBases, GitLabProvider, TokenInfo};
use providers::{BbClient, Provider, ProviderError};
use store::{
    email_key, token_key, Account, RepoChoice, SecretStore, Store, StoreReader, StoreWriter,
    LEGACY_BB_EMAIL, LEGACY_BB_TOKEN, META_REPOS_PRESELECTED,
};

use crate::accounts::bitbucket_keys;
use sync::{run_discovery_for, run_sync, DiscoveryError, SyncEvent, SyncOptions};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use prcore::comments::{
    AnchorModel, Comment, NewComment, PostRevision, ReviewAction, ServerRevision, ThreadId,
};
use providers::provider::set_review_checked;

use crate::lock::lock_ok;
use crate::review::composer::{CommentPoster, PostError, Poster};
use crate::review::{CommentFetcher, DepsFn, HeadProbe, ReviewDeps, ReviewService};

pub enum Msg {
    Connected(UserRef),
    /// 401 from `me()` or from the sync: email or token must be re-entered (both stay stored).
    AuthRejected,
    Failed(String),
    Sync(SyncEvent),
    SyncFailed(String),
    /// A fresh copy of the cache, built on the runtime (`request_snapshot`); `Err` when the
    /// cache could not be read.
    Snapshot(Result<Snapshot, String>),
    /// A repository discovery finished (`first`: the one a first sign-in runs).
    Discovery {
        first: bool,
        result: Result<Discovered, String>,
    },
    /// The discovery cache with the selection (`request_repo_choices`).
    RepoChoices(Result<Vec<RepoChoice>, String>),
    /// `save_selection` was written (or not).
    SelectionSaved(Result<(), String>),
    /// `add_bitbucket_account` / `add_github_account` finished: the new account's name, or
    /// why it was not added.
    AccountAdded(Result<String, String>),
    /// `test_github_connection` finished: who the token signs in as, or why not.
    ConnectionTested(Result<String, String>),
    /// `remove_account` finished: what was removed, or why not.
    AccountRemoved(Result<String, String>),
    /// `set_account_enabled` / `rename_account` finished: what changed, or why not.
    AccountUpdated(Result<String, String>),
    /// After a sync: the repositories, not selected, where open PRs request `account`'s
    /// review, with the number of PRs (a hint only; nothing is selected).
    RequestedElsewhere {
        account: AccountId,
        repos: Vec<(RepoRef, usize)>,
    },
}

/// What a discovery found, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub repos: usize,
    /// Repositories selected after it (none on a first sign-in).
    pub selected: usize,
}

/// User-facing text of a failed discovery (categories only, never detail).
fn discovery_error(e: &DiscoveryError) -> String {
    match e {
        DiscoveryError::Auth => "authentication failed".to_string(),
        DiscoveryError::Workspaces(cat) => format!("Could not list your repositories ({cat})"),
        DiscoveryError::Store => "Could not store the repository list".to_string(),
    }
}

/// Runs a discovery of every enabled account and reports it once: the repositories listed
/// by all of them. A 401 of the default (signed-in) account is `AuthRejected` and stops; any
/// other account's failure is left on its own error rows and does not stop the others.
/// Returns false after the default account's 401.
async fn discover_and_report(
    reg: &ProviderRegistry,
    store: &Mutex<Store>,
    first: bool,
    send: &(impl Fn(Msg) + Sync),
) -> bool {
    let mut repos = 0;
    let mut listed_any = false;
    let mut first_error = None;
    for account in sync_targets(store, true) {
        let primary = account == AccountId::default_bitbucket();
        let outcome = match reg.get(&account) {
            Ok(p) => run_discovery_for(p.as_ref(), store, &account).await,
            Err(msg) => {
                first_error.get_or_insert(msg);
                continue;
            }
        };
        match outcome {
            Err(DiscoveryError::Auth) if primary => {
                send(Msg::AuthRejected);
                return false;
            }
            Err(e) => {
                if !primary {
                    record_account_error(store, &account, &discovery_error(&e));
                }
                first_error.get_or_insert(discovery_error(&e));
            }
            Ok(d) => {
                listed_any = true;
                repos += d.repos;
            }
        }
    }
    let result = match (listed_any, first_error) {
        (false, Some(e)) => Err(e),
        _ => {
            let selected = lock_ok(store)
                .selected_repos()
                .map(|v| v.len())
                .unwrap_or(0);
            Ok(Discovered { repos, selected })
        }
    };
    send(Msg::Discovery { first, result });
    true
}

/// The accounts a sync or discovery covers: every enabled account (the default one first),
/// the default one only when `with_default`. A cache without any account row still has the
/// default sign-in (credentials saved before the row was written).
fn sync_targets(store: &Mutex<Store>, with_default: bool) -> Vec<AccountId> {
    let default = AccountId::default_bitbucket();
    let accounts = lock_ok(store).accounts().unwrap_or_default();
    let mut out: Vec<AccountId> = Vec::new();
    if with_default && accounts.iter().all(|a| a.id != default || a.enabled) {
        out.push(default.clone());
    }
    out.extend(
        accounts
            .into_iter()
            .filter(|a| a.enabled && a.id != default)
            .map(|a| a.id),
    );
    out
}

/// An account-wide problem (credentials missing, 401) on the account's own listing row, so
/// the inbox says which account failed while the others keep syncing.
fn record_account_error(store: &Mutex<Store>, account: &AccountId, msg: &str) {
    let pseudo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "*".into(),
        slug: "*".into(),
    };
    let _ = lock_ok(store).set_repo_error_for(account, &pseudo, Some(msg));
}

/// Per-account sync concurrency (spec 4.3): Bitbucket 8, GitHub and GitLab 4.
fn concurrency_for(forge: Forge) -> usize {
    match forge {
        Forge::BitbucketCloud => 8,
        Forge::GitHub | Forge::GitLab => 4,
    }
}

/// Syncs `accounts` one after the other, each with its own provider and options, as one
/// sync for the UI: one `Started`, the repository count summed, one `Finished`. A failing
/// account (no credentials, 401, ...) is recorded on its own rows and the next one still
/// syncs. Returns the default account's result (its 401 asks for a new sign-in).
async fn sync_accounts(
    reg: &ProviderRegistry,
    store: &Arc<Mutex<Store>>,
    accounts: Vec<AccountId>,
    events: &UnboundedSender<SyncEvent>,
) -> Result<(), ProviderError> {
    let _ = events.send(SyncEvent::Started);
    let mut total = 0;
    let mut primary_result = Ok(());
    for account in accounts {
        let primary = account == AccountId::default_bitbucket();
        let provider = match reg.get(&account) {
            Ok(p) => p,
            Err(msg) => {
                record_account_error(store, &account, &msg);
                continue;
            }
        };
        let opts = SyncOptions {
            account: account.clone(),
            concurrency: concurrency_for(provider.forge()),
            ..SyncOptions::default()
        };
        let (etx, mut erx) = unbounded_channel::<SyncEvent>();
        let fwd = events.clone();
        let base = total;
        let forward = tokio::spawn(async move {
            let mut repos = 0;
            while let Some(ev) = erx.recv().await {
                match ev {
                    SyncEvent::Started | SyncEvent::Finished => {}
                    SyncEvent::Discovered { repos: n } => {
                        repos = n;
                        let _ = fwd.send(SyncEvent::Discovered { repos: base + n });
                    }
                    ev => {
                        let _ = fwd.send(ev);
                    }
                }
            }
            repos
        });
        let result = run_sync(provider, store.clone(), opts, etx).await;
        total += forward.await.unwrap_or(0);
        match result {
            Ok(()) if !primary => record_account_clear(store, &account),
            Ok(()) => {}
            Err(e) if primary => primary_result = Err(e),
            Err(e) => record_account_error(store, &account, &e.category()),
        }
    }
    let _ = events.send(SyncEvent::Finished);
    primary_result
}

/// Clears an account-wide error once the account synced again.
fn record_account_clear(store: &Mutex<Store>, account: &AccountId) {
    let pseudo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "*".into(),
        slug: "*".into(),
    };
    let _ = lock_ok(store).set_repo_error_for(account, &pseudo, None);
}

/// A new, random, non-secret account id (`acc-` and 16 hex digits).
fn new_account_id() -> AccountId {
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
fn add_account(
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
fn add_github(
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
fn github_signin_error(e: &ProviderError) -> String {
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
async fn github_connection_summary(p: &GitHubProvider) -> Result<String, String> {
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
fn expiry_key(id: &AccountId) -> String {
    format!("account.{}.token_expires", id.as_str())
}

/// When `id`'s token expires, as recorded at sign-in (GitLab only).
pub fn token_expiry(store: &Store, id: &AccountId) -> Option<chrono::NaiveDate> {
    store
        .get_meta(&expiry_key(id))
        .ok()
        .flatten()
        .and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
}

/// Stores a checked GitLab sign-in as a new account: the token only (read back), then the
/// account row with the derived bases and the token's expiry date. Refuses a user that
/// already has an account on the same host.
fn add_gitlab(
    secrets: &dyn SecretStore,
    store: &Mutex<Store>,
    bases: &GitLabBases,
    token: &str,
    me: &UserRef,
    expires: Option<chrono::NaiveDate>,
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
        let _ = lock_ok(store).set_meta(&expiry_key(&id), &d.format("%Y-%m-%d").to_string());
    }
    Ok((id, format!("{shown} ({})", bases.git_host)))
}

/// What a failed GitLab sign-in means for the user (never a URL or the token).
fn gitlab_signin_error(e: &ProviderError) -> String {
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
fn gitlab_token_summary(
    who: &str,
    info: &TokenInfo,
    today: chrono::NaiveDate,
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
async fn gitlab_connection_summary(p: &GitLabProvider) -> Result<String, String> {
    let me = p.me().await.map_err(|e| gitlab_signin_error(&e))?;
    let repos = match p.discover().await {
        Ok(d) => format!("{} projects visible", d.repos.len()),
        Err(e) => format!("listing projects failed: {}", e.user_message()),
    };
    let who = me.login.unwrap_or(me.display_name);
    let today = chrono::Local::now().date_naive();
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
fn remove_account(
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
fn label_key(id: &AccountId) -> String {
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
fn set_enabled(store: &Store, id: &AccountId, enabled: bool) -> Result<(), String> {
    store
        .set_account_enabled(id, enabled)
        .map_err(|_| "Could not change the account".to_string())
}

/// The accounts whose client is paused by a rate limit right now, with until when.
fn paused_accounts(reg: &ProviderRegistry) -> Vec<(AccountId, std::time::SystemTime)> {
    let mut out: Vec<_> = lock_ok(&reg.by_account)
        .iter()
        .filter_map(|(id, p)| p.paused_until().map(|t| (id.clone(), t)))
        .collect();
    out.sort();
    out
}

/// The inbox status line's rate-limit note ("GitHub (mona): paused until 14:05"), for the
/// pauses that have not ended; None when there is none.
pub fn paused_line(paused: &[(Forge, String, std::time::SystemTime)]) -> Option<String> {
    let now = std::time::SystemTime::now();
    let parts: Vec<String> = paused
        .iter()
        .filter(|(_, _, until)| *until > now)
        .map(|(forge, label, until)| {
            format!(
                "{} ({label}): paused until {}",
                forge.label(),
                clock(*until)
            )
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The repositories of `found` (one entry per PR asking for my review) that no account
/// syncs, with how many PRs each: the "requested elsewhere" hint (decision D4). Reads the
/// selection only; never selects anything.
fn requested_elsewhere(store: &Store, found: &[RepoRef]) -> Vec<(RepoRef, usize)> {
    let selected: std::collections::HashSet<String> = store
        .selected_repos()
        .unwrap_or_default()
        .iter()
        .map(RepoRef::key)
        .collect();
    let mut counts: std::collections::BTreeMap<String, (RepoRef, usize)> = Default::default();
    for r in found.iter().filter(|r| !selected.contains(&r.key())) {
        counts.entry(r.key()).or_insert_with(|| (r.clone(), 0)).1 += 1;
    }
    counts.into_values().collect()
}

/// The inbox hint for `repos` (`requested_elsewhere` of every account), naming at most five.
pub fn elsewhere_hint(repos: &[(RepoRef, usize)]) -> Option<String> {
    let prs: usize = repos.iter().map(|(_, n)| n).sum();
    if prs == 0 {
        return None;
    }
    let names: Vec<String> = repos
        .iter()
        .take(5)
        .map(|(r, _)| match r.host.as_str() {
            "github.com" => r.full_name(),
            host => format!("{} ({host})", r.full_name()),
        })
        .collect();
    let mut list = names.join(", ");
    if repos.len() > names.len() {
        list = format!("{list} and {} more", repos.len() - names.len());
    }
    Some(if prs == 1 {
        format!(
            "1 open pull request requests your review in a repository you have not selected: \
             {list}"
        )
    } else {
        let where_ = if repos.len() == 1 {
            "a repository"
        } else {
            "repositories"
        };
        format!(
            "{prs} open pull requests request your review in {where_} you have not selected: \
             {list}"
        )
    })
}

/// Asks every enabled account in `accounts` for review requests outside the selection (at
/// most once per sync; the provider also bounds it in time) and reports each answer.
async fn report_requested_elsewhere(
    reg: &ProviderRegistry,
    store: &Mutex<Store>,
    accounts: &[AccountId],
    send: &(impl Fn(Msg) + Sync),
) {
    for account in accounts {
        let Ok(p) = reg.get(account) else { continue };
        // None: not offered, or asked too recently (the UI keeps the last answer)
        if let Ok(Some(found)) = p.review_requests_elsewhere().await {
            let repos = requested_elsewhere(&lock_ok(store), &found);
            send(Msg::RequestedElsewhere {
                account: account.clone(),
                repos,
            });
        }
    }
}

/// A local wall-clock time, `HH:MM`.
pub fn clock(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%H:%M")
        .to_string()
}

/// Shown instead of an empty inbox when the cache cannot be read.
pub const CACHE_READ_FAILED: &str = "Could not read the cache";

/// What the UI shows, copied out of the store.
#[derive(Default, Debug, PartialEq)]
pub struct Snapshot {
    pub prs: Vec<PullRequest>,
    pub stats: HashMap<(RepoRef, u64), DiffStat>,
    pub errors: Vec<(RepoRef, String)>,
    /// Selected repositories (only their PRs and errors are in the snapshot).
    pub repo_count: usize,
    /// Accessible repositories in the discovery cache.
    pub known_repos: usize,
    /// Repositories the upgrade selected because PRs of them were cached, until the notice
    /// about it is dismissed.
    pub preselected: Option<usize>,
    /// The user the default account was last signed in as (`Store::adopt_user`); the UI
    /// drops a snapshot of another user than the signed-in one.
    pub account: Option<String>,
    /// Each account's own user id, for per-account "Mine" and "Needs me".
    pub account_users: HashMap<AccountId, String>,
}

pub struct Backend {
    rt: tokio::runtime::Runtime,
    ctx: eframe::egui::Context,
    secrets: Arc<dyn SecretStore>,
    store: Arc<Mutex<Store>>,
    tx: UnboundedSender<Msg>,
    pub rx: UnboundedReceiver<Msg>,
    /// The connect-and-sync task in flight; at most one runs at a time.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// A discovery asked for from the repository picker (`discover`).
    discovery: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// One provider per account (all on the shared connection pool), used by sync, review
    /// and posting.
    providers: Arc<ProviderRegistry>,
    /// The one review service (and its worker pool), made on first use.
    review: std::sync::OnceLock<Arc<ReviewService>>,
    /// A snapshot build is running / another one was asked for meanwhile.
    snap_busy: Arc<AtomicBool>,
    snap_again: Arc<AtomicBool>,
    /// The channel snapshots are sent into, taken at each build: `sign_out` replaces it, so
    /// a build that was already running answers into the new session's channel.
    snap_tx: Arc<Mutex<UnboundedSender<Msg>>>,
    /// Settings writes, off the UI thread.
    writer: StoreWriter,
    /// The inbox snapshot's reads (the store's read connection, outside `store`'s mutex).
    reader: StoreReader,
}

impl Backend {
    pub fn new(
        ctx: eframe::egui::Context,
        secrets: Arc<dyn SecretStore>,
        store: Store,
    ) -> Result<Backend, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|_| "could not start the background runtime".to_string())?;
        let (tx, rx) = unbounded_channel();
        let reader = store.reader();
        let store = Arc::new(Mutex::new(store));
        let providers = Arc::new(ProviderRegistry::new(secrets.clone(), store.clone()));
        Ok(Backend {
            rt,
            ctx,
            secrets,
            writer: StoreWriter::new(store.clone()),
            reader,
            store,
            snap_tx: Arc::new(Mutex::new(tx.clone())),
            tx,
            rx,
            task: Mutex::new(None),
            discovery: Mutex::new(None),
            providers,
            review: std::sync::OnceLock::new(),
            snap_busy: Arc::new(AtomicBool::new(false)),
            snap_again: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Opens `cache.sqlite` in the data folder (`brand::data_dir`), creating the directory
    /// (on Windows the old `prview` folder while it could not be moved, see `brand`). Saved
    /// folders that pointed into the moved folder are pointed at the new one.
    pub fn open_store() -> Result<Store, String> {
        let base = crate::brand::data_base().ok_or(crate::brand::DATA_DIR_UNSET)?;
        let dir = crate::brand::data_dir_at(&base);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create cache folder: {e}"))?;
        let store = Store::open(&dir.join("cache.sqlite"))
            .map_err(|e| format!("cannot open cache: {e}"))?;
        if cfg!(windows) {
            crate::brand::rebase_settings(
                &store,
                &base,
                &crate::settings_ui::default_download_dir(),
            );
        }
        Ok(store)
    }

    pub fn has_stored_credentials(&self) -> bool {
        let (ek, tk) = bitbucket_keys();
        matches!(self.secrets.get(&ek), Ok(Some(_))) && matches!(self.secrets.get(&tk), Ok(Some(_)))
    }

    /// Saves the credentials, then connects with them. The caller clears its fields on `Ok`.
    pub fn save_and_connect(&self, email: &str, token: &str) -> Result<(), String> {
        let (ek, tk) = bitbucket_keys();
        self.secrets
            .set(&ek, email)
            .and_then(|_| self.secrets.set(&tk, token))
            .map_err(|e| format!("Could not save credentials: {e}"))?;
        if !crate::accounts::ensure_default_account(&lock_ok(&self.store)) {
            return Err("Could not record the account in the cache".into());
        }
        self.providers.clear();
        self.connect_stored();
        Ok(())
    }

    /// Connects the default account with whatever the secret store holds (reads happen on
    /// the runtime), then syncs every enabled account. A task still in flight is aborted
    /// first, so two syncs never run at once. When the default account cannot connect, the
    /// other accounts still sync.
    pub fn connect_stored(&self) {
        let reg = self.providers.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let handle = self.rt.spawn(async move {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let primary = AccountId::default_bitbucket();
            let mut primary_ok = false;
            match reg.get(&primary) {
                Err(msg) => send(Msg::Failed(msg)),
                Ok(provider) => match provider.me().await {
                    Ok(me) => {
                        // before the first sync: another user's rows of this account are
                        // not shown or mixed in
                        if let Err(msg) = adopt_account(&store, &primary, &me) {
                            return send(Msg::Failed(msg));
                        }
                        send(Msg::Connected(me));
                        primary_ok = true;
                    }
                    Err(ProviderError::Auth) => send(Msg::AuthRejected),
                    Err(e) => send(Msg::Failed(format!(
                        "Could not connect ({}). Showing cached data.",
                        e.user_message()
                    ))),
                },
            }

            // The first sign-in lists the repositories to choose from; nothing is synced
            // until some are selected (the sync below then covers none).
            let first = primary_ok && !lock_ok(&store).discovered_any().unwrap_or(true);
            if first && !discover_and_report(&reg, &store, true, &send).await {
                return;
            }

            let accounts = sync_targets(&store, primary_ok);
            if accounts.is_empty() {
                return;
            }
            let (etx, mut erx) = unbounded_channel::<SyncEvent>();
            let fwd_tx = tx.clone();
            let fwd_ctx = ctx.clone();
            let forward = tokio::spawn(async move {
                while let Some(ev) = erx.recv().await {
                    let _ = fwd_tx.send(Msg::Sync(ev));
                    fwd_ctx.request_repaint();
                }
            });
            let result = sync_accounts(&reg, &store, accounts.clone(), &etx).await;
            drop(etx);
            let _ = forward.await;
            // once per sync: review requests in repositories nobody selected (a hint)
            report_requested_elsewhere(&reg, &store, &accounts, &send).await;
            match result {
                Ok(()) => {}
                Err(ProviderError::Auth) => send(Msg::AuthRejected),
                Err(e) => send(Msg::SyncFailed(format!("Sync failed: {e}"))),
            }
        });
        let mut slot = lock_ok(&self.task);
        if let Some(old) = slot.replace(handle) {
            old.abort();
        }
    }

    /// Lists the repositories of every enabled account again (no PR calls); the result
    /// arrives as `Msg::Discovery`. A discovery still running is replaced.
    pub fn discover(&self) {
        let reg = self.providers.clone();
        let store = self.store.clone();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let handle = self.rt.spawn(async move {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            discover_and_report(&reg, &store, false, &send).await;
        });
        if let Some(old) = lock_ok(&self.discovery).replace(handle) {
            old.abort();
        }
    }

    /// Reads the discovery cache with the selection on the runtime's blocking pool; arrives
    /// as `Msg::RepoChoices`.
    pub fn request_repo_choices(&self) {
        let (store, tx, ctx) = (self.store.clone(), self.tx.clone(), self.ctx.clone());
        self.rt.spawn_blocking(move || {
            let choices = lock_ok(&store)
                .repo_choices()
                .map_err(|_| "Could not read the repository list".to_string());
            let _ = tx.send(Msg::RepoChoices(choices));
            ctx.request_repaint();
        });
    }

    /// Makes `selected` the repositories to sync, each through its account, and deletes the
    /// cached PRs of `purge`, on the writer thread; the outcome arrives as
    /// `Msg::SelectionSaved`.
    pub fn save_selection(&self, selected: Vec<(AccountId, RepoRef)>, purge: Vec<RepoRef>) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.writer.submit(move |s| {
            let result = s
                .set_selection_all(&selected)
                .and_then(|_| match purge.is_empty() {
                    true => Ok(()),
                    false => s.purge_repos(&purge),
                })
                .map_err(|_| "Could not save the repository selection".to_string());
            let _ = tx.send(Msg::SelectionSaved(result));
            ctx.request_repaint();
        });
    }

    /// Every account (no secrets), ordered by id; empty when the store cannot be read.
    pub fn accounts(&self) -> Vec<Account> {
        lock_ok(&self.store).accounts().unwrap_or_default()
    }

    /// Adds another Bitbucket Cloud account: checks the email and token with `me()` (on
    /// the runtime), refuses an account that is already added, stores the credentials under
    /// a new account id, then lists its repositories. The outcome arrives as
    /// `Msg::AccountAdded`; the token is only in the credential store and the client.
    pub fn add_bitbucket_account(&self, email: String, token: String) {
        let (secrets, store, reg) = (
            self.secrets.clone(),
            self.store.clone(),
            self.providers.clone(),
        );
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let template = Account::default_bitbucket(String::new(), String::new());
            let probe = BitbucketProvider::new(BbClient::new(&template.api_base, &email, &token));
            let result = match probe.me().await {
                Err(ProviderError::Auth) => Err("The email or API token was not accepted".into()),
                Err(e) => Err(format!("Could not connect ({})", e.user_message())),
                Ok(me) => add_account(secrets.as_ref(), &store, &email, &token, &me),
            };
            drop(token);
            if let Ok((id, _)) = &result {
                if let Ok(p) = reg.get(id) {
                    let _ = run_discovery_for(p.as_ref(), &store, id).await;
                }
            }
            let _ = tx.send(Msg::AccountAdded(result.map(|(_, name)| name)));
            ctx.request_repaint();
        });
    }

    /// Adds a GitHub account (read-only in this version): checks the token with `me()` on
    /// the derived API base, refuses one already added, stores the token under a new
    /// account id, then lists its repositories. The outcome arrives as `Msg::AccountAdded`.
    pub fn add_github_account(&self, bases: GitHubBases, token: String) {
        let (secrets, store, reg) = (
            self.secrets.clone(),
            self.store.clone(),
            self.providers.clone(),
        );
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let probe =
                GitHubProvider::new(&bases.api_base, &bases.graphql_url, &bases.git_host, &token);
            let result = match probe.me().await {
                Err(e) => Err(github_signin_error(&e)),
                Ok(me) => add_github(secrets.as_ref(), &store, &bases, &token, &me),
            };
            drop(token);
            if let Ok((id, _)) = &result {
                if let Ok(p) = reg.get(id) {
                    let _ = run_discovery_for(p.as_ref(), &store, id).await;
                }
            }
            let _ = tx.send(Msg::AccountAdded(result.map(|(_, name)| name)));
            ctx.request_repaint();
        });
    }

    /// "Test connection" for a GitHub token: signs in and lists repositories, stores
    /// nothing. The answer arrives as `Msg::ConnectionTested`.
    pub fn test_github_connection(&self, bases: GitHubBases, token: String) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let probe =
                GitHubProvider::new(&bases.api_base, &bases.graphql_url, &bases.git_host, &token);
            drop(token);
            let result = github_connection_summary(&probe).await;
            let _ = tx.send(Msg::ConnectionTested(result));
            ctx.request_repaint();
        });
    }

    /// Adds a GitLab account: checks the token with `me()` on
    /// the derived API base, reads its expiry, refuses one already added, stores the token
    /// under a new account id, then lists its projects. The outcome arrives as
    /// `Msg::AccountAdded`.
    pub fn add_gitlab_account(&self, bases: GitLabBases, token: String) {
        let (secrets, store, reg) = (
            self.secrets.clone(),
            self.store.clone(),
            self.providers.clone(),
        );
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let probe = GitLabProvider::new(&bases.api_base, &bases.git_host, &token);
            let result = match probe.me().await {
                Err(e) => Err(gitlab_signin_error(&e)),
                Ok(me) => {
                    let expires = probe.token_info().await.ok().and_then(|i| i.expires_at);
                    add_gitlab(secrets.as_ref(), &store, &bases, &token, &me, expires)
                }
            };
            drop(token);
            if let Ok((id, _)) = &result {
                if let Ok(p) = reg.get(id) {
                    let _ = run_discovery_for(p.as_ref(), &store, id).await;
                }
            }
            let _ = tx.send(Msg::AccountAdded(result.map(|(_, name)| name)));
            ctx.request_repaint();
        });
    }

    /// "Test connection" for a GitLab token: signs in, reads the token's scopes and expiry
    /// and lists projects; stores nothing. The answer arrives as `Msg::ConnectionTested`.
    pub fn test_gitlab_connection(&self, bases: GitLabBases, token: String) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let probe = GitLabProvider::new(&bases.api_base, &bases.git_host, &token);
            drop(token);
            let result = gitlab_connection_summary(&probe).await;
            let _ = tx.send(Msg::ConnectionTested(result));
            ctx.request_repaint();
        });
    }

    /// Removes an account other than the signed-in default one (that one signs out): its
    /// credentials, its repository choices and its cached rows; its drafts stay. The outcome
    /// arrives as `Msg::AccountRemoved`.
    pub fn remove_account(&self, id: AccountId) {
        let (secrets, store, reg) = (
            self.secrets.clone(),
            self.store.clone(),
            self.providers.clone(),
        );
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn_blocking(move || {
            let result = remove_account(secrets.as_ref(), &store, &reg, &id);
            let _ = tx.send(Msg::AccountRemoved(result));
            ctx.request_repaint();
        });
    }

    /// Enables or disables an account on the writer thread; its selection and cache stay.
    /// The outcome arrives as `Msg::AccountUpdated`.
    pub fn set_account_enabled(&self, id: AccountId, enabled: bool) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.writer.submit(move |s| {
            let result = set_enabled(s, &id, enabled).map(|_| {
                if enabled {
                    "Enabled: it syncs again from the next refresh".to_string()
                } else {
                    "Disabled: not synced, its repository choice is kept".to_string()
                }
            });
            let _ = tx.send(Msg::AccountUpdated(result));
            ctx.request_repaint();
        });
    }

    /// Gives an account a label (empty: back to its login), on the writer thread. The
    /// outcome arrives as `Msg::AccountUpdated`.
    pub fn rename_account(&self, id: AccountId, label: String) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.writer.submit(move |s| {
            let result = set_account_label(s, &id, &label)
                .map(|_| "Renamed".to_string())
                .map_err(|_| "Could not rename the account".to_string());
            let _ = tx.send(Msg::AccountUpdated(result));
            ctx.request_repaint();
        });
    }

    /// Every account's label (`account_label`), in store order.
    pub fn account_labels(&self) -> Vec<(AccountId, String)> {
        let guard = lock_ok(&self.store);
        guard
            .accounts()
            .unwrap_or_default()
            .iter()
            .map(|a| (a.id.clone(), account_label(&guard, a)))
            .collect()
    }

    /// The accounts paused by a rate limit right now: (forge, account, until). Reads only the
    /// providers (never the store), so the UI thread may call it after any sync event.
    pub fn paused_status(&self) -> Vec<(Forge, AccountId, std::time::SystemTime)> {
        let forges: HashMap<AccountId, Forge> = lock_ok(&self.providers.by_account)
            .iter()
            .map(|(id, p)| (id.clone(), p.forge()))
            .collect();
        paused_accounts(&self.providers)
            .into_iter()
            .filter_map(|(id, until)| Some((*forges.get(&id)?, id, until)))
            .collect()
    }

    /// The notice about repositories the upgrade selected is not shown again.
    pub fn dismiss_preselected_notice(&self) {
        self.writer
            .write("Could not save that the notice was dismissed", |s| {
                s.delete_meta(META_REPOS_PRESELECTED)
            });
    }

    /// Stops any connect or sync, deletes the stored email, token and `me`, and drops
    /// messages still queued from the old session. The PR cache itself is kept.
    pub fn sign_out(&mut self) -> Result<(), String> {
        for slot in [&self.task, &self.discovery] {
            if let Some(t) = lock_ok(slot).take() {
                t.abort();
            }
        }
        let (tx, rx) = unbounded_channel();
        *lock_ok(&self.snap_tx) = tx.clone();
        self.tx = tx;
        self.rx = rx;
        self.providers.clear();
        // the account's keys, and the pre-account ones should a move have failed (else
        // the next start would sign in again with them)
        let (ek, tk) = bitbucket_keys();
        let secrets = [tk.as_str(), ek.as_str(), LEGACY_BB_TOKEN, LEGACY_BB_EMAIL]
            .into_iter()
            .try_for_each(|k| self.secrets.delete(k))
            .map_err(|e| format!("Could not remove saved credentials: {e}"));
        let guard = lock_ok(&self.store);
        let me = guard
            .delete_meta(META_ME)
            .map_err(|_| "Could not clear the saved user".to_string());
        secrets.and(me)
    }

    /// Runs `f` with the store locked (settings reads and writes; keep it short).
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        f(&lock_ok(&self.store))
    }

    /// The user remembered from the last successful `me()`, so cached views work offline.
    pub fn stored_me(&self) -> Option<UserRef> {
        let guard = lock_ok(&self.store);
        load_me(&guard)
    }

    pub fn stored_email(&self) -> Option<String> {
        self.secrets.get(&bitbucket_keys().0).ok().flatten()
    }

    /// The review service, shared by every review of this backend (one worker pool). Each
    /// open reads the stored credentials for git, `Provider::pr()` as the head probe and
    /// `Provider::comments()` as the comment fetcher on the service's own thread (both run
    /// on this runtime). Also returns the store.
    pub fn review_service(&self) -> (Arc<ReviewService>, Arc<Mutex<Store>>) {
        let svc = self.review.get_or_init(|| {
            let secrets = self.secrets.clone();
            let handle = self.rt.handle().clone();
            let reg = self.providers.clone();
            let deps: DepsFn =
                Arc::new(move |repo: &RepoRef| review_deps(secrets.as_ref(), &reg, &handle, repo));
            Arc::new(ReviewService::with_deps(
                self.store.clone(),
                self.ctx.clone(),
                deps,
            ))
        });
        (svc.clone(), self.store.clone())
    }

    /// Posts comments and (un)resolves threads through the provider on this runtime (called
    /// from the review's worker threads, never the UI thread). Reads no credentials here:
    /// the shared provider is built from them on the first call, on that worker thread; a
    /// call without stored credentials sends nothing and fails as not sent.
    pub fn comment_poster(&self) -> Option<Poster> {
        Some(Arc::new(ProviderPoster {
            reg: self.providers.clone(),
            rt: self.rt.handle().clone(),
        }))
    }

    /// Asks for a fresh snapshot of the cache. It is built on the runtime's blocking pool
    /// (never on the caller's thread, which does not touch the store) and arrives as
    /// `Msg::Snapshot`. Requests made while one is being built coalesce into one more build,
    /// so the last request is always answered with data at least as new as itself.
    pub fn request_snapshot(&self) {
        self.snap_again.store(true, Ordering::SeqCst);
        if self.snap_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let (reader, tx, ctx) = (self.reader.clone(), self.snap_tx.clone(), self.ctx.clone());
        let (busy, again) = (self.snap_busy.clone(), self.snap_again.clone());
        self.rt.spawn_blocking(move || {
            let read = || read_snapshot(&reader).map_err(|_| CACHE_READ_FAILED.to_string());
            snapshot_builds(read, &tx, &ctx, &busy, &again);
        });
    }

    /// The writer thread for UI-initiated store writes (settings).
    pub fn writer(&self) -> &StoreWriter {
        &self.writer
    }

    #[cfg(test)]
    pub(crate) fn store_handle(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    #[cfg(test)]
    pub(crate) fn test_sender(&self) -> UnboundedSender<Msg> {
        self.tx.clone()
    }
}

impl Drop for Backend {
    /// Clean shutdown: queued writes are stored, then the WAL is folded into the file.
    fn drop(&mut self) {
        self.writer.flush();
        let _ = self.with_store(|s| s.checkpoint());
    }
}

/// Builds snapshots while `again` is set, each sent into the channel `tx` holds at the time
/// it is sent (not the one of the request that started the loop), then clears `busy`.
fn snapshot_builds(
    read: impl Fn() -> Result<Snapshot, String>,
    tx: &Mutex<UnboundedSender<Msg>>,
    ctx: &eframe::egui::Context,
    busy: &AtomicBool,
    again: &AtomicBool,
) {
    loop {
        while again.swap(false, Ordering::SeqCst) {
            let snap = read();
            let tx = lock_ok(tx).clone();
            let _ = tx.send(Msg::Snapshot(snap));
            ctx.request_repaint();
        }
        busy.store(false, Ordering::SeqCst);
        // a request that came in after the last check but saw `busy` still set
        if !again.load(Ordering::SeqCst) || busy.swap(true, Ordering::SeqCst) {
            break;
        }
    }
}

/// The inbox's data: one joined query for PRs and diffstats, errors and the repo count, in one
/// read transaction, on the store's read connection: it never waits for the `Store` mutex
/// that the sync and the writers hold (WAL readers do not wait for writers).
pub fn read_snapshot(reader: &StoreReader) -> Result<Snapshot, store::StoreError> {
    let c = reader.cache_snapshot()?;
    let mut prs = Vec::with_capacity(c.prs.len());
    let mut stats = HashMap::new();
    for (p, stat) in c.prs {
        if let Some((_, d)) = stat {
            stats.insert((p.repo.clone(), p.id), d);
        }
        prs.push(p);
    }
    Ok(Snapshot {
        prs,
        stats,
        errors: c.errors,
        repo_count: c.repo_count,
        known_repos: c.known_repos,
        preselected: c.preselected,
        account: c.account_id,
        account_users: c.account_users.into_iter().collect(),
    })
}

/// What one review open of `repo` needs from the sign-in of the account it belongs to
/// (blocking secret reads: call it off the UI thread).
fn review_deps(
    secrets: &dyn SecretStore,
    reg: &ProviderRegistry,
    handle: &tokio::runtime::Handle,
    repo: &RepoRef,
) -> Arc<ReviewDeps> {
    let owner = reg.owner(repo);
    let auth = reg.account(&owner).and_then(|a| git_auth(secrets, &a));
    let provider = reg.get(&owner).ok();
    let (p2, h1, h2) = (provider.clone(), handle.clone(), handle.clone());
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
    Arc::new(ReviewDeps {
        auth,
        probe,
        comments,
    })
}

/// Git credentials of `account`, scoped to its git host (spec 7.2). Bitbucket needs both
/// stored values (a half sign-in is no sign-in); git authenticates with the token alone.
fn git_auth(secrets: &dyn SecretStore, account: &Account) -> Option<GitAuth> {
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
struct ProviderRegistry {
    secrets: Arc<dyn SecretStore>,
    store: Arc<Mutex<Store>>,
    by_account: Mutex<HashMap<AccountId, Arc<dyn Provider>>>,
}

impl ProviderRegistry {
    fn new(secrets: Arc<dyn SecretStore>, store: Arc<Mutex<Store>>) -> Self {
        ProviderRegistry {
            secrets,
            store,
            by_account: Mutex::new(HashMap::new()),
        }
    }

    /// The account row; the default sign-in also without one (saved before the row was).
    fn account(&self, id: &AccountId) -> Option<Account> {
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
    fn owner(&self, repo: &RepoRef) -> AccountId {
        lock_ok(&self.store)
            .repo_account(repo)
            .ok()
            .flatten()
            .unwrap_or_else(AccountId::default_bitbucket)
    }

    /// Blocking secret reads on the first call per account: keep it off the UI thread.
    fn get(&self, id: &AccountId) -> Result<Arc<dyn Provider>, String> {
        if let Some(p) = lock_ok(&self.by_account).get(id) {
            return Ok(p.clone());
        }
        let account = self.account(id).ok_or("No such account")?;
        let p = build_provider(&account, self.secrets.as_ref())?;
        lock_ok(&self.by_account).insert(id.clone(), p.clone());
        Ok(p)
    }

    /// The provider of the account `repo` is reached through.
    fn for_repo(&self, repo: &RepoRef) -> Result<Arc<dyn Provider>, String> {
        self.get(&self.owner(repo))
    }

    fn clear(&self) {
        lock_ok(&self.by_account).clear();
    }
}

/// Builds `account`'s provider; the token exists only inside this function and the client.
fn build_provider(
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

/// `CommentPoster` backed by the provider of the repository's account; blocks the calling
/// worker thread.
struct ProviderPoster {
    reg: Arc<ProviderRegistry>,
    rt: tokio::runtime::Handle,
}

impl ProviderPoster {
    /// The provider of `repo`'s account (blocking secret reads on first use: worker threads
    /// only).
    fn provider(&self, repo: &RepoRef) -> Result<Arc<dyn Provider>, PostError> {
        self.reg
            .for_repo(repo)
            .map_err(|_| PostError::Network("not signed in".into()))
    }
}

/// Category text only: no URLs, bodies or credentials reach the UI.
fn post_error(e: &ProviderError) -> PostError {
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

const META_ME: &str = "me";

/// Stores only uuid and display name (no secrets).
fn save_me(store: &Mutex<Store>, me: &UserRef) {
    let guard = lock_ok(store);
    let _ = guard.set_meta(META_ME, &format!("{}\n{}", me.id, me.display_name));
}

/// Records that `account` is signed in as `me`: when it was another user before, that
/// account's rebuildable rows go first (drafts stay, other accounts are untouched, see
/// `Store::adopt_user`). The default account's `me` is remembered for offline views.
fn adopt_account(store: &Mutex<Store>, account: &AccountId, me: &UserRef) -> Result<(), String> {
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

fn load_me(store: &Store) -> Option<UserRef> {
    let raw = store.get_meta(META_ME).ok().flatten()?;
    let (uuid, name) = raw.split_once('\n')?;
    (!uuid.is_empty()).then(|| UserRef {
        id: uuid.into(),
        display_name: name.into(),
        login: None,
    })
}

#[cfg(test)]
mod leak_check;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use store::MemorySecrets;

    #[test]
    fn me_round_trips_through_meta() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load_me(&store), None);
        let me = UserRef {
            id: "{abc-123}".into(),
            display_name: "Ana Souza".into(),
            login: None,
        };
        let m = Mutex::new(store);
        save_me(&m, &me);
        assert_eq!(load_me(&m.lock().unwrap()), Some(me));
    }

    pub(crate) fn backend_with_saved_login() -> (Backend, Arc<MemorySecrets>) {
        let secrets = Arc::new(MemorySecrets::new());
        let (ek, tk) = bitbucket_keys();
        secrets.set(&ek, "ana@example.com").unwrap();
        secrets.set(&tk, "not-a-real-token").unwrap();
        let b = Backend::new(
            eframe::egui::Context::default(),
            secrets.clone(),
            Store::open_in_memory().unwrap(),
        )
        .unwrap();
        let me = UserRef {
            id: "{abc}".into(),
            display_name: "Ana".into(),
            login: None,
        };
        save_me(&b.store, &me);
        (b, secrets)
    }

    #[test]
    fn a_pre_account_sign_in_survives_the_upgrade() {
        let secrets = Arc::new(MemorySecrets::new());
        secrets.set(LEGACY_BB_EMAIL, "ana@example.com").unwrap();
        secrets.set(LEGACY_BB_TOKEN, "not-a-real-token").unwrap();
        let store = Store::open_in_memory().unwrap();
        // as the app's start does, before the backend
        let moved = crate::accounts::migrate_legacy_bitbucket(secrets.as_ref(), &store);
        assert_eq!(moved, crate::accounts::MigrateOutcome::Moved);
        let mut b = Backend::new(eframe::egui::Context::default(), secrets.clone(), store).unwrap();
        assert!(b.has_stored_credentials(), "still signed in");
        assert_eq!(b.stored_email().as_deref(), Some("ana@example.com"));
        assert_eq!(secrets.get(LEGACY_BB_TOKEN).unwrap(), None);
        // a legacy entry left by a failed move is removed by signing out too
        secrets.set(LEGACY_BB_TOKEN, "left-over").unwrap();
        b.sign_out().unwrap();
        assert_eq!(secrets.get(LEGACY_BB_TOKEN).unwrap(), None);
        assert!(!b.has_stored_credentials());
    }

    #[test]
    fn sign_out_deletes_email_token_and_me() {
        let (mut b, secrets) = backend_with_saved_login();
        assert!(b.has_stored_credentials() && b.stored_me().is_some());
        b.sign_out().unwrap();
        let (ek, tk) = bitbucket_keys();
        assert_eq!(secrets.get(&ek).unwrap(), None);
        assert_eq!(secrets.get(&tk).unwrap(), None);
        assert!(!b.has_stored_credentials());
        assert_eq!(b.stored_me(), None);
    }

    #[test]
    fn sign_out_drops_messages_from_the_old_session() {
        let (mut b, _) = backend_with_saved_login();
        let _ = b.tx.send(Msg::Connected(b.stored_me().unwrap()));
        b.sign_out().unwrap();
        assert!(b.rx.try_recv().is_err());
    }

    /// Counts reads; the UI thread must never trigger one through `comment_poster`.
    struct CountingSecrets {
        inner: MemorySecrets,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl SecretStore for CountingSecrets {
        fn get(&self, key: &str) -> Result<Option<String>, store::SecretError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), store::SecretError> {
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), store::SecretError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn comment_poster_reads_no_credentials_on_the_calling_thread() {
        let secrets = Arc::new(CountingSecrets {
            inner: MemorySecrets::new(),
            reads: Default::default(),
        });
        let b = Backend::new(
            eframe::egui::Context::default(),
            secrets.clone(),
            Store::open_in_memory().unwrap(),
        )
        .unwrap();
        let poster = b.comment_poster().expect("a poster");
        assert_eq!(
            secrets.reads.load(Ordering::SeqCst),
            0,
            "no read on the UI thread"
        );
        // the first call (on a worker) reads them; without credentials it sends nothing
        let repo = RepoRef::parse_bitbucket("acme/api").unwrap();
        let err =
            std::thread::spawn(move || poster.set_review(&repo, 1, ReviewAction::Approve, None))
                .join()
                .unwrap()
                .unwrap_err();
        assert!(secrets.reads.load(Ordering::SeqCst) > 0);
        assert!(matches!(err, PostError::Network(_)), "{err:?}");
    }

    #[test]
    fn post_errors_map_to_categories_without_detail() {
        let secret = "https://x?token=secret";
        let cases = [
            (ProviderError::PostedUnreadable, PostError::PostedUnreadable),
            (
                ProviderError::Network(secret.into()),
                PostError::Network("network error".into()),
            ),
            (
                ProviderError::Invalid(secret.into()),
                PostError::Invalid("invalid request".into()),
            ),
            (
                ProviderError::Forbidden,
                PostError::Rejected("access denied".into()),
            ),
            (
                ProviderError::Decode(secret.into()),
                PostError::Rejected("decode error".into()),
            ),
            (ProviderError::Unconfirmed, PostError::Unconfirmed),
            (
                ProviderError::Rejected {
                    status: 400,
                    message: "You can't approve your own pull request.".into(),
                },
                PostError::Rejected("http 400: You can't approve your own pull request.".into()),
            ),
            (
                ProviderError::Rejected {
                    status: 403,
                    message: "Insufficient scope".into(),
                },
                PostError::Rejected("access denied: Insufficient scope".into()),
            ),
        ];
        for (e, want) in cases {
            assert_eq!(post_error(&e), want);
        }
        let (b, _) = backend_with_saved_login();
        assert!(b.comment_poster().is_some());
    }

    /// The inbox read as it was before the joined query: `list_prs` plus one `get_diffstat`
    /// per PR (the reference the joined query must match).
    fn n_plus_one_snapshot(store: &Store) -> Snapshot {
        let prs = store.list_prs().unwrap();
        let mut stats = HashMap::new();
        for p in &prs {
            if let Ok(Some((_, d))) = store.get_diffstat(&p.repo, p.id) {
                stats.insert((p.repo.clone(), p.id), d);
            }
        }
        Snapshot {
            prs,
            stats,
            errors: store.repo_errors().unwrap(),
            repo_count: store.selected_repos().unwrap().len(),
            known_repos: store.repo_choices().unwrap().len(),
            preselected: None,
            account: store.get_meta("account_id").unwrap(),
            account_users: HashMap::new(),
        }
    }

    #[test]
    fn a_snapshot_build_running_at_sign_out_answers_into_the_new_channel() {
        let (old_tx, mut old_rx) = unbounded_channel();
        let slot = Arc::new(Mutex::new(old_tx));
        let (busy, again) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let (s, b, a) = (slot.clone(), busy.clone(), again.clone());
        let builder = std::thread::spawn(move || {
            let read = || {
                let _ = entered_tx.send(());
                let _ = go_rx.lock().unwrap().recv();
                Ok(Snapshot::default())
            };
            snapshot_builds(read, &s, &eframe::egui::Context::default(), &b, &a);
        });
        entered_rx.recv().unwrap();
        // sign-out while the first build runs, then a request of the new session
        let (new_tx, mut new_rx) = unbounded_channel();
        *slot.lock().unwrap() = new_tx;
        again.store(true, Ordering::SeqCst);
        go_tx.send(()).unwrap();
        entered_rx.recv().unwrap();
        go_tx.send(()).unwrap();
        builder.join().unwrap();
        assert!(matches!(new_rx.try_recv(), Ok(Msg::Snapshot(Ok(_)))));
        assert!(matches!(new_rx.try_recv(), Ok(Msg::Snapshot(Ok(_)))));
        assert!(
            old_rx.try_recv().is_err(),
            "sent into the old session's channel"
        );
        assert!(!busy.load(Ordering::SeqCst));
    }

    /// A provider answering from memory: one open PR per repository, or `Auth` everywhere;
    /// records the calls it gets.
    struct FakeProvider {
        auth_fails: bool,
        calls: Mutex<Vec<String>>,
        /// What `paused_until` reports (a rate-limited client).
        paused: Option<std::time::SystemTime>,
    }

    impl FakeProvider {
        fn new(auth_fails: bool) -> Arc<FakeProvider> {
            Arc::new(FakeProvider {
                auth_fails,
                calls: Mutex::new(Vec::new()),
                paused: None,
            })
        }
        fn note(&self, c: String) -> Result<(), ProviderError> {
            lock_ok(&self.calls).push(c);
            if self.auth_fails {
                Err(ProviderError::Auth)
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn forge(&self) -> Forge {
            Forge::BitbucketCloud
        }
        fn capabilities(&self) -> providers::provider::Capabilities {
            providers::provider::Capabilities::BITBUCKET
        }
        fn paused_until(&self) -> Option<std::time::SystemTime> {
            self.paused
        }
        async fn me(&self) -> Result<UserRef, ProviderError> {
            self.note("me".into())?;
            Ok(UserRef {
                id: "{me}".into(),
                display_name: "Me".into(),
                login: None,
            })
        }
        async fn discover(&self) -> Result<providers::provider::Discovered, ProviderError> {
            self.note("discover".into())?;
            Ok(Default::default())
        }
        async fn prs(
            &self,
            r: &RepoRef,
            _: prcore::model::PrState,
            _: usize,
        ) -> Result<providers::provider::PrList, ProviderError> {
            self.note(format!("prs:{}", r.full_name()))?;
            let mut pr = crate::demo::demo_data().1[0].clone();
            pr.repo = r.clone();
            pr.source_repo = r.clone();
            pr.id = 1;
            Ok(providers::provider::PrList {
                prs: vec![pr],
                skipped: vec![],
            })
        }
        async fn merged_by_source_branch(
            &self,
            _: &RepoRef,
            _: &str,
        ) -> Result<providers::provider::PrList, ProviderError> {
            Ok(Default::default())
        }
        async fn diffstat(&self, _: &RepoRef, _: u64) -> Result<DiffStat, ProviderError> {
            Ok(DiffStat {
                files: 1,
                lines_added: 1,
                lines_removed: 0,
                max_file_lines: 1,
            })
        }
        async fn pr(&self, r: &RepoRef, id: u64) -> Result<PullRequest, ProviderError> {
            self.note(format!("pr:{}#{id}", r.full_name()))?;
            Err(ProviderError::NotFound)
        }
        async fn revision(&self, _: &RepoRef, _: u64) -> Result<ServerRevision, ProviderError> {
            Err(ProviderError::NotFound)
        }
        async fn comments(
            &self,
            _: &RepoRef,
            _: u64,
        ) -> Result<providers::provider::CommentList, ProviderError> {
            Ok(Default::default())
        }
        async fn tasks(
            &self,
            _: &RepoRef,
            _: u64,
        ) -> Result<Vec<prcore::comments::PrTask>, ProviderError> {
            Ok(vec![])
        }
        async fn post_comment(
            &self,
            r: &RepoRef,
            id: u64,
            _: &PostRevision,
            _: &NewComment,
        ) -> Result<Comment, ProviderError> {
            self.note(format!("post:{}#{id}", r.full_name()))?;
            Err(ProviderError::Unconfirmed)
        }
        async fn set_resolved(
            &self,
            _: &RepoRef,
            _: u64,
            _: &ThreadId,
            _: bool,
        ) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn set_review(
            &self,
            _: &RepoRef,
            _: u64,
            _: ReviewAction,
            _: Option<&str>,
        ) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn second_account() -> Account {
        Account {
            id: AccountId("acc-second".into()),
            ..Account::default_bitbucket("{bo}".into(), "Bo".into())
        }
    }

    /// A store with the default and a second Bitbucket account, each with one selected
    /// repository, and a registry answering with the given fakes (no credentials needed).
    fn two_accounts(
        default: &Arc<FakeProvider>,
        second: Option<&Arc<FakeProvider>>,
    ) -> (Arc<Mutex<Store>>, ProviderRegistry, RepoRef, RepoRef) {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = (
            RepoRef::parse_bitbucket("acme/api").unwrap(),
            RepoRef::parse_bitbucket("beta/web").unwrap(),
        );
        store
            .upsert_account(&Account::default_bitbucket("{ana}".into(), "Ana".into()))
            .unwrap();
        store.upsert_account(&second_account()).unwrap();
        store.set_selection(std::slice::from_ref(&a)).unwrap();
        store
            .set_selection_for(&second_account().id, std::slice::from_ref(&b))
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let reg = ProviderRegistry::new(Arc::new(MemorySecrets::new()), store.clone());
        {
            let mut map = lock_ok(&reg.by_account);
            map.insert(AccountId::default_bitbucket(), default.clone());
            if let Some(p) = second {
                map.insert(second_account().id, p.clone());
            }
        }
        (store, reg, a, b)
    }

    fn run_accounts(
        reg: &ProviderRegistry,
        store: &Arc<Mutex<Store>>,
    ) -> (Result<(), ProviderError>, Vec<SyncEvent>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = unbounded_channel();
        let accounts = sync_targets(store, true);
        let res = rt.block_on(sync_accounts(reg, store, accounts, &tx));
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        (res, events)
    }

    #[test]
    fn two_accounts_sync_independently_and_auth_failure_stops_only_one() {
        // the default account's token is rejected: the second account still syncs
        let (bad, good) = (FakeProvider::new(true), FakeProvider::new(false));
        let (store, reg, a, b) = two_accounts(&bad, Some(&good));
        let (res, events) = run_accounts(&reg, &store);
        assert_eq!(
            res,
            Err(ProviderError::Auth),
            "the sign-in screen is asked for"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SyncEvent::Started | SyncEvent::Finished))
                .count(),
            2,
            "one sync for the UI: {events:?}"
        );
        assert_eq!(
            lock_ok(&bad.calls).clone(),
            vec![format!("prs:{}", a.full_name())]
        );
        assert_eq!(
            lock_ok(&good.calls).clone(),
            vec![format!("prs:{}", b.full_name())]
        );
        let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
        assert_eq!(snap.prs.len(), 1);
        assert_eq!(snap.prs[0].repo, b);
        assert_eq!(snap.prs[0].account, second_account().id);
        // the other way round: the second account's 401 is its own error row only
        let (good, bad) = (FakeProvider::new(false), FakeProvider::new(true));
        let (store, reg, a, _) = two_accounts(&good, Some(&bad));
        let (res, events) = run_accounts(&reg, &store);
        assert_eq!(res, Ok(()));
        assert!(
            events.contains(&SyncEvent::Discovered { repos: 2 }),
            "{events:?}"
        );
        let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
        assert_eq!(snap.prs.len(), 1);
        assert_eq!(snap.prs[0].repo, a);
        assert_eq!(
            snap.errors
                .iter()
                .map(|(_, m)| m.as_str())
                .collect::<Vec<_>>(),
            vec!["authentication failed"]
        );
    }

    #[test]
    fn an_account_without_credentials_does_not_block_the_others() {
        let good = FakeProvider::new(false);
        let (store, reg, a, _) = two_accounts(&good, None);
        let (res, _) = run_accounts(&reg, &store);
        assert_eq!(res, Ok(()));
        let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
        assert_eq!(snap.prs.len(), 1);
        assert_eq!(snap.prs[0].repo, a);
        assert_eq!(
            snap.errors
                .iter()
                .map(|(_, m)| m.as_str())
                .collect::<Vec<_>>(),
            vec!["No stored credentials"]
        );
        // a disabled account is not synced at all
        lock_ok(&store)
            .set_account_enabled(&second_account().id, false)
            .unwrap();
        assert_eq!(
            sync_targets(&store, true),
            vec![AccountId::default_bitbucket()]
        );
        assert_eq!(sync_targets(&store, false), Vec::<AccountId>::new());
    }

    #[test]
    fn two_github_accounts_same_host_coexist() {
        let secrets = MemorySecrets::new();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let bases = providers::github::bases_for("https://github.com").unwrap();
        let user = |id: &str, login: &str| UserRef {
            id: id.into(),
            display_name: format!("{login} name"),
            login: Some(login.into()),
        };
        let (work, _) =
            add_github(&secrets, &store, &bases, "tok-work", &user("1", "mona")).unwrap();
        let (home, _) =
            add_github(&secrets, &store, &bases, "tok-home", &user("2", "octocat")).unwrap();
        assert_ne!(work, home);
        assert_eq!(
            secrets.get(&token_key(&work)).unwrap().as_deref(),
            Some("tok-work")
        );
        assert_eq!(
            secrets.get(&token_key(&home)).unwrap().as_deref(),
            Some("tok-home")
        );
        let accounts = lock_ok(&store).accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts
            .iter()
            .all(|a| a.forge == Forge::GitHub && a.git_host == "github.com"));
        // each has its own label: the login until renamed; a rename touches one only
        let label = |id: &AccountId| {
            let a = accounts.iter().find(|a| a.id == *id).unwrap();
            account_label(&lock_ok(&store), a)
        };
        assert_eq!(
            (label(&work), label(&home)),
            ("mona".into(), "octocat".into())
        );
        set_account_label(&lock_ok(&store), &work, "  Work  ").unwrap();
        assert_eq!(
            (label(&work), label(&home)),
            ("Work".into(), "octocat".into())
        );
        set_account_label(&lock_ok(&store), &work, "").unwrap();
        assert_eq!(
            label(&work),
            "mona",
            "an empty label falls back to the login"
        );
        set_account_label(&lock_ok(&store), &work, "Work").unwrap();
        // both sync, each through its own provider
        let store = Arc::new(store);
        assert_eq!(sync_targets(&store, false).len(), 2);
        for a in &accounts {
            assert_eq!(build_provider(a, &secrets).unwrap().forge(), Forge::GitHub);
        }
        // removing one leaves the other's token, row and label
        let reg = ProviderRegistry::new(Arc::new(MemorySecrets::new()), store.clone());
        remove_account(&secrets, &store, &reg, &work).unwrap();
        assert_eq!(secrets.get(&token_key(&work)).unwrap(), None);
        assert_eq!(
            secrets.get(&token_key(&home)).unwrap().as_deref(),
            Some("tok-home")
        );
        assert_eq!(lock_ok(&store).get_meta(&label_key(&work)).unwrap(), None);
        let left = lock_ok(&store).accounts().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(account_label(&lock_ok(&store), &left[0]), "octocat");
    }

    #[test]
    fn disabled_account_not_synced_but_selection_kept() {
        let (first, second) = (FakeProvider::new(false), FakeProvider::new(false));
        let (store, reg, a, b) = two_accounts(&first, Some(&second));
        let id = second_account().id;
        set_enabled(&lock_ok(&store), &id, false).unwrap();
        let (res, _) = run_accounts(&reg, &store);
        assert_eq!(res, Ok(()));
        assert!(
            lock_ok(&second.calls).is_empty(),
            "a disabled account sends nothing"
        );
        assert_eq!(
            lock_ok(&first.calls).clone(),
            vec![format!("prs:{}", a.full_name())]
        );
        // its selection is still there, still through it
        let selected = lock_ok(&store).selected_repos_by_account().unwrap();
        assert!(selected.contains(&(id.clone(), b.clone())), "{selected:?}");
        assert!(!lock_ok(&store)
            .accounts()
            .unwrap()
            .iter()
            .any(|x| x.id == id && x.enabled));
        // enabled again: it syncs its kept selection
        set_enabled(&lock_ok(&store), &id, true).unwrap();
        assert_eq!(run_accounts(&reg, &store).0, Ok(()));
        assert_eq!(
            lock_ok(&second.calls).clone(),
            vec![format!("prs:{}", b.full_name())]
        );
        assert!(set_enabled(&lock_ok(&store), &AccountId("acc-nope".into()), false).is_err());
    }

    #[test]
    fn paused_status_shown_per_account() {
        let until = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
        let calm = FakeProvider::new(false);
        let paused = Arc::new(FakeProvider {
            auth_fails: false,
            calls: Mutex::new(Vec::new()),
            paused: Some(until),
        });
        let (store, reg, _, _) = two_accounts(&calm, Some(&paused));
        let second = second_account().id;
        assert_eq!(paused_accounts(&reg), vec![(second.clone(), until)]);
        // the Accounts card: the paused account says so, the other one does not
        let accounts = lock_ok(&store).accounts().unwrap();
        let mut others = crate::settings_ui::other_accounts(&accounts);
        for o in &mut others {
            o.paused_until = paused_accounts(&reg)
                .into_iter()
                .find(|(id, _)| *id == o.id)
                .map(|(_, t)| t);
        }
        let hhmm = chrono::DateTime::<chrono::Local>::from(until)
            .format("%H:%M")
            .to_string();
        let (text, tone) = crate::settings_ui::other_account_status(&others[0]);
        assert_eq!(text, format!("Paused by the rate limit until {hhmm}"));
        assert_eq!(tone, crate::settings_ui::Tone::Warn);
        // the inbox status line names the account
        let line = paused_line(&[(Forge::GitHub, "mona".into(), until)]).unwrap();
        assert_eq!(line, format!("GitHub (mona): paused until {hhmm}"));
        let two = paused_line(&[
            (Forge::GitHub, "mona".into(), until),
            (Forge::GitLab, "alex".into(), until),
        ])
        .unwrap();
        assert_eq!(
            two,
            format!("GitHub (mona): paused until {hhmm}; GitLab (alex): paused until {hhmm}")
        );
        assert_eq!(paused_line(&[]), None);
        // a pause that ended is not shown
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
        assert_eq!(paused_line(&[(Forge::GitHub, "mona".into(), past)]), None);
    }

    #[test]
    fn remove_confirm_counts_match_store() {
        let store = Store::open_in_memory().unwrap();
        let gh = Account {
            id: AccountId("acc-gh".into()),
            forge: Forge::GitHub,
            git_host: "github.com".into(),
            ..second_account()
        };
        store.upsert_account(&gh).unwrap();
        let mine = RepoRef::parse("github.com", "octo/api").unwrap();
        let other = RepoRef::parse_bitbucket("acme/web").unwrap();
        store
            .record_discovery_for(
                &gh.id,
                &[prcore::model::RepoInfo::from(mine.clone())],
                &[],
                None,
            )
            .unwrap();
        store
            .set_selection_for(&gh.id, std::slice::from_ref(&mine))
            .unwrap();
        store.set_selection(std::slice::from_ref(&other)).unwrap();
        let template = crate::demo::demo_data().1[0].clone();
        let prs: Vec<PullRequest> = (1..=3)
            .map(|id| {
                let mut p = template.clone();
                p.repo = mine.clone();
                p.id = id;
                p.account = gh.id.clone();
                p
            })
            .collect();
        let stat = DiffStat {
            files: 1,
            lines_added: 1,
            lines_removed: 0,
            max_file_lines: 1,
        };
        let stats = vec![(1, "c1".to_string(), stat.clone()), (2, "c2".into(), stat)];
        store
            .apply_repo_sync_for(&gh.id, &mine, Some(&prs), &stats, None)
            .unwrap();
        store
            .apply_repo_sync(&other, Some(std::slice::from_ref(&template)), &[], None)
            .unwrap();
        // two open drafts on the account's repository, one posted, one of another account
        let draft = |repo: &RepoRef, text: &str| prcore::draft::Draft {
            id: 0,
            repo: repo.clone(),
            pr_id: 1,
            kind: prcore::draft::DraftKind::Pr,
            path: None,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text: text.into(),
            source_commit: "c1".into(),
            comment_type: None,
        };
        store.save_draft(&draft(&mine, "a")).unwrap();
        store.save_draft(&draft(&mine, "b")).unwrap();
        let posted = store.save_draft(&draft(&mine, "c")).unwrap();
        store.mark_draft_posted(posted).unwrap();
        store.save_draft(&draft(&other, "d")).unwrap();

        let preview = store.removal_counts(&gh.id).unwrap();
        assert_eq!(
            crate::settings_ui::removal_summary(&preview),
            "Removes 3 cached PRs, keeps 2 drafts marked"
        );
        let before = store.list_prs().unwrap().len();
        let removed = store.remove_account(&gh.id).unwrap();
        assert_eq!(
            removed, preview,
            "the confirmation said exactly what happened"
        );
        assert_eq!(store.list_prs().unwrap().len(), before - preview.prs);
        assert_eq!(store.list_drafts(&mine, 1).unwrap().len(), 3, "drafts stay");
        // nothing of it is left but the kept drafts
        assert_eq!(
            store.removal_counts(&gh.id).unwrap(),
            store::RemovedCounts {
                drafts_kept: 2,
                ..Default::default()
            }
        );
        assert_eq!(
            crate::settings_ui::removal_summary(&store::RemovedCounts {
                prs: 1,
                drafts_kept: 1,
                ..Default::default()
            }),
            "Removes 1 cached PR, keeps 1 draft marked"
        );
    }

    #[test]
    fn hint_lists_only_unselected_repos() {
        let store = Store::open_in_memory().unwrap();
        let r = |n: &str| RepoRef::parse("github.com", n).unwrap();
        let gh = AccountId("acc-gh".into());
        store
            .record_discovery_for(
                &gh,
                &[
                    prcore::model::RepoInfo::from(r("octo/api")),
                    prcore::model::RepoInfo::from(r("other/lib")),
                ],
                &[],
                None,
            )
            .unwrap();
        store.set_selection_for(&gh, &[r("octo/api")]).unwrap();
        // selected through another account (D8): not "elsewhere" either
        store
            .set_selection_for(&AccountId("acc-gh2".into()), &[r("octo/web")])
            .unwrap();
        let before = store.repo_choices().unwrap();
        let ghe = RepoRef::parse("ghe.corp.example", "octo/api").unwrap();
        let found = vec![
            r("octo/api"),
            r("other/lib"),
            r("octo/web"),
            r("other/lib"),
            r("zed/app"),
            ghe.clone(),
        ];
        let hint = requested_elsewhere(&store, &found);
        assert_eq!(
            hint,
            vec![(ghe, 1), (r("other/lib"), 2), (r("zed/app"), 1)],
            "only unselected repositories (the same path on another host is another one)"
        );
        assert_eq!(
            store.repo_choices().unwrap(),
            before,
            "the hint never selects (or lists) anything"
        );
        assert_eq!(
            elsewhere_hint(&hint).as_deref(),
            Some(
                "4 open pull requests request your review in repositories you have not \
                 selected: octo/api (ghe.corp.example), other/lib, zed/app"
            )
        );
        let many: Vec<(RepoRef, usize)> = (0..7).map(|i| (r(&format!("o/r{i}")), 1)).collect();
        assert!(elsewhere_hint(&many).unwrap().ends_with("o/r4 and 2 more"));
        let one = [(r("zed/app"), 1)];
        assert_eq!(
            elsewhere_hint(&one).as_deref(),
            Some("1 open pull request requests your review in a repository you have not selected: zed/app")
        );
        assert_eq!(elsewhere_hint(&[]), None);
        assert!(requested_elsewhere(&store, &[r("octo/api")]).is_empty());
    }

    #[test]
    fn poster_routes_to_owning_account() {
        let (first, second) = (FakeProvider::new(false), FakeProvider::new(false));
        let (_store, reg, a, b) = two_accounts(&first, Some(&second));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let poster = ProviderPoster {
            reg: Arc::new(reg),
            rt: rt.handle().clone(),
        };
        let text = NewComment::Pr { text: "x".into() };
        let at = PostRevision::default();
        let on_b = b.clone();
        let p = &poster;
        std::thread::scope(|s| {
            s.spawn(|| {
                let _ = p.post(&on_b, 7, &at, &text);
                let _ = p.post(&a, 3, &at, &text);
            });
        });
        assert_eq!(lock_ok(&second.calls).clone(), vec!["post:beta/web#7"]);
        assert_eq!(lock_ok(&first.calls).clone(), vec!["post:acme/api#3"]);
    }

    #[test]
    fn review_deps_use_account_scope() {
        let secrets = MemorySecrets::new();
        let second = second_account();
        secrets
            .set(&email_key(&second.id), "bo@example.com")
            .unwrap();
        secrets.set(&token_key(&second.id), "tok-of-bo").unwrap();
        let auth = git_auth(&secrets, &second).expect("Bitbucket git auth");
        let want = GitAuth::bitbucket("tok-of-bo");
        assert_eq!(
            (auth.user, auth.token, auth.scope),
            (want.user, want.token, want.scope)
        );
        // the default account has nothing stored: no git credentials
        let default = Account::default_bitbucket(String::new(), String::new());
        assert!(git_auth(&secrets, &default).is_none());
        // never for another host than the account's own
        let elsewhere = Account {
            git_host: "bitbucket.example".into(),
            ..second.clone()
        };
        assert!(git_auth(&secrets, &elsewhere).is_none());
        // GitHub: x-access-token on the account's own git host; GitLab: oauth2 on its host
        // (a relative URL root's prefix is not part of the scope)
        let gh = Account {
            forge: Forge::GitHub,
            git_host: "ghe.corp.example".into(),
            ..second.clone()
        };
        let a = git_auth(&secrets, &gh).expect("GitHub git auth");
        assert_eq!(
            (a.user.as_str(), a.scope.as_str()),
            ("x-access-token", "https://ghe.corp.example/")
        );
        let gl = Account {
            forge: Forge::GitLab,
            git_host: "git.corp.example/gitlab".into(),
            ..second
        };
        let a = git_auth(&secrets, &gl).expect("GitLab git auth");
        assert_eq!(
            (a.user.as_str(), a.scope.as_str()),
            ("oauth2", "https://git.corp.example/")
        );
        // and a provider is never built for an http API address
        let insecure = Account {
            api_base: "http://api.bitbucket.org/2.0".into(),
            ..second_account()
        };
        assert!(build_provider(&insecure, &secrets).is_err());
    }

    #[test]
    fn an_http_account_on_a_private_address_gets_cleartext_git_auth_and_a_provider() {
        let secrets = MemorySecrets::new();
        let gl = providers::gitlab::bases_for("http://192.168.1.20:8080").unwrap();
        let acc = Account {
            forge: Forge::GitLab,
            web_base: gl.web_base,
            api_base: gl.api_base,
            git_host: gl.git_host,
            ..second_account()
        };
        secrets.set(&token_key(&acc.id), "glpat-tok").unwrap();
        let a = git_auth(&secrets, &acc).expect("cleartext git auth");
        assert_eq!(
            (a.user.as_str(), a.scope.as_str()),
            ("oauth2", "http://192.168.1.20:8080/")
        );
        assert!(!format!("{a:?}").contains("glpat-tok"));
        assert!(build_provider(&acc, &secrets).is_ok());
        let gh = providers::github::bases_for("http://ghe.localhost:8080").unwrap();
        let ghacc = Account {
            forge: Forge::GitHub,
            web_base: gh.web_base,
            api_base: gh.api_base,
            graphql_base: Some(gh.graphql_url),
            git_host: gh.git_host,
            ..acc.clone()
        };
        let a = git_auth(&secrets, &ghacc).expect("cleartext git auth");
        assert_eq!(a.scope, "http://ghe.localhost:8080/");
        assert!(build_provider(&ghacc, &secrets).is_ok());
        // a public http address (e.g. a hand-edited cache): no git credential, no provider
        let public = Account {
            web_base: "http://gitlab.example".into(),
            api_base: "http://gitlab.example/api/v4".into(),
            git_host: "gitlab.example".into(),
            ..acc.clone()
        };
        assert!(git_auth(&secrets, &public).is_none());
        assert!(build_provider(&public, &secrets).is_err());
        // an https account on a private address stays https
        let tls = Account {
            web_base: "https://192.168.1.20:8443".into(),
            api_base: "https://192.168.1.20:8443/api/v4".into(),
            git_host: "192.168.1.20:8443".into(),
            ..acc
        };
        assert_eq!(
            git_auth(&secrets, &tls).unwrap().scope,
            "https://192.168.1.20:8443/"
        );
    }

    #[test]
    fn an_added_account_gets_its_own_keys_and_removal_takes_only_its_rows() {
        let secrets = MemorySecrets::new();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        save_me(
            &store,
            &UserRef {
                id: "{ana}".into(),
                display_name: "Ana".into(),
                login: None,
            },
        );
        let bo = UserRef {
            id: "{bo}".into(),
            display_name: "Bo".into(),
            login: None,
        };
        let (id, name) = add_account(&secrets, &store, "bo@example.com", "tok-bo", &bo).unwrap();
        assert_eq!(name, "Bo");
        assert!(id.as_str().starts_with("acc-") && id != AccountId::default_bitbucket());
        assert_eq!(
            secrets.get(&token_key(&id)).unwrap().as_deref(),
            Some("tok-bo")
        );
        let accounts = lock_ok(&store).accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].user_id, "{bo}");
        assert!(!format!("{accounts:?}").contains("tok-bo"));
        // the same user again, or the signed-in one, is refused
        assert!(add_account(&secrets, &store, "bo@example.com", "t", &bo).is_err());
        let ana = UserRef {
            id: "{ana}".into(),
            display_name: "Ana".into(),
            login: None,
        };
        assert!(add_account(&secrets, &store, "ana@example.com", "t", &ana).is_err());
        // removal: its keys and rows, nothing of the default account
        let r = RepoRef::parse_bitbucket("beta/web").unwrap();
        let a = RepoRef::parse_bitbucket("acme/api").unwrap();
        {
            let st = lock_ok(&store);
            st.set_selection_for(&id, std::slice::from_ref(&r)).unwrap();
            st.set_selection(std::slice::from_ref(&a)).unwrap();
        }
        let reg = ProviderRegistry::new(
            Arc::new(MemorySecrets::new()),
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        );
        assert!(remove_account(&secrets, &store, &reg, &AccountId::default_bitbucket()).is_err());
        let msg = remove_account(&secrets, &store, &reg, &id).unwrap();
        assert!(msg.starts_with("Removed"), "{msg}");
        assert_eq!(secrets.get(&token_key(&id)).unwrap(), None);
        assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
        assert_eq!(lock_ok(&store).selected_repos().unwrap(), vec![a]);
        assert!(lock_ok(&store).accounts().unwrap().is_empty());
    }

    #[test]
    fn a_github_account_stores_only_its_token_and_its_derived_bases() {
        let secrets = MemorySecrets::new();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let bases = providers::github::bases_for("https://ghe.corp.example").unwrap();
        let me = UserRef {
            id: "42".into(),
            display_name: "Mona".into(),
            login: Some("mona".into()),
        };
        let (id, name) = add_github(&secrets, &store, &bases, "gh-tok", &me).unwrap();
        assert_eq!(name, "mona (ghe.corp.example)");
        assert_eq!(
            secrets.get(&token_key(&id)).unwrap().as_deref(),
            Some("gh-tok")
        );
        assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
        let a = lock_ok(&store).accounts().unwrap().pop().unwrap();
        assert_eq!(a.forge, Forge::GitHub);
        assert_eq!(a.api_base, "https://ghe.corp.example/api/v3");
        assert_eq!(
            a.graphql_base.as_deref(),
            Some("https://ghe.corp.example/api/graphql")
        );
        assert_eq!(
            (a.git_host.as_str(), a.login.as_str()),
            ("ghe.corp.example", "mona")
        );
        assert_eq!(a.auth_kind, "pat");
        assert!(!format!("{a:?}").contains("gh-tok"));
        // the same user on the same host is refused; on github.com it is another account
        assert!(add_github(&secrets, &store, &bases, "t2", &me).is_err());
        let dotcom = providers::github::bases_for("https://github.com").unwrap();
        assert!(add_github(&secrets, &store, &dotcom, "t3", &me).is_ok());
        // and its provider is built from the stored token
        let p = build_provider(&a, &secrets).unwrap();
        assert_eq!(p.forge(), Forge::GitHub);
        assert!(!p.capabilities().read_only);
        let insecure = Account {
            graphql_base: Some("http://ghe.corp.example/api/graphql".into()),
            ..a.clone()
        };
        assert!(build_provider(&insecure, &secrets).is_err());
        let _ = secrets.delete(&token_key(&a.id));
        assert!(build_provider(&a, &secrets).is_err());
    }

    #[test]
    fn a_gitlab_account_stores_only_its_token_and_its_derived_bases() {
        let secrets = MemorySecrets::new();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let bases = providers::gitlab::bases_for("https://git.corp.example/gitlab").unwrap();
        let me = UserRef {
            id: "17".into(),
            display_name: "Alex".into(),
            login: Some("alex".into()),
        };
        let expires = chrono::NaiveDate::from_ymd_opt(2027, 1, 31);
        let (id, name) = add_gitlab(&secrets, &store, &bases, "glpat-tok", &me, expires).unwrap();
        assert_eq!(name, "alex (git.corp.example/gitlab)");
        assert_eq!(
            secrets.get(&token_key(&id)).unwrap().as_deref(),
            Some("glpat-tok")
        );
        assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
        let a = lock_ok(&store).accounts().unwrap().pop().unwrap();
        assert_eq!(a.forge, Forge::GitLab);
        assert_eq!(a.api_base, "https://git.corp.example/gitlab/api/v4");
        assert_eq!(a.graphql_base, None);
        assert_eq!(a.git_host, "git.corp.example/gitlab");
        assert!(!format!("{a:?}").contains("glpat-tok"));
        // the expiry is kept (no secret) for the Accounts card's warning
        assert_eq!(token_expiry(&lock_ok(&store), &id), expires);
        assert!(add_gitlab(&secrets, &store, &bases, "t2", &me, None).is_err());
        // its provider is built from the stored token and can write
        let p = build_provider(&a, &secrets).unwrap();
        assert_eq!(p.forge(), Forge::GitLab);
        assert!(!p.capabilities().read_only);
        // removal takes the expiry with the keys
        let reg = ProviderRegistry::new(
            Arc::new(MemorySecrets::new()),
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        );
        remove_account(&secrets, &store, &reg, &id).unwrap();
        assert_eq!(token_expiry(&lock_ok(&store), &id), None);
        assert!(build_provider(&a, &secrets).is_err());
    }

    #[test]
    fn gitlab_signin_errors_and_summary_never_carry_the_token() {
        let text = gitlab_signin_error(&ProviderError::Auth);
        assert!(text.contains("did not accept"), "{text}");
        let info = providers::gitlab::TokenInfo {
            scopes: vec!["read_api".into(), "read_repository".into()],
            expires_at: chrono::NaiveDate::from_ymd_opt(2026, 11, 20),
            active: true,
        };
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        let s = gitlab_token_summary("alex", &info, today, "3 projects visible");
        assert!(s.contains("Signed in as alex"), "{s}");
        assert!(s.contains("expires in 6 day(s)"), "{s}");
        assert!(s.contains("api scope"), "{s}");
    }

    #[test]
    fn github_signin_errors_never_carry_a_url() {
        let sso = ProviderError::Rejected {
            status: 403,
            message: providers::github::SSO_HINT.into(),
        };
        for e in [sso, ProviderError::Auth, ProviderError::RateLimited] {
            let text = github_signin_error(&e);
            assert!(!text.contains("https://"), "{text}");
        }
        assert!(github_signin_error(&ProviderError::Auth).contains("did not accept"));
    }

    #[test]
    fn new_account_ids_differ() {
        let ids: std::collections::HashSet<AccountId> = (0..50).map(|_| new_account_id()).collect();
        assert_eq!(ids.len(), 50);
    }

    #[test]
    fn signing_in_as_another_account_clears_the_cache_first() {
        let store = Store::open_in_memory().unwrap();
        populate(&store, 2, 2);
        let m = Mutex::new(store);
        let ana = UserRef {
            id: "{ana}".into(),
            display_name: "Ana".into(),
            login: None,
        };
        adopt_account(&m, &AccountId::default_bitbucket(), &ana).unwrap();
        populate(&m.lock().unwrap(), 2, 2);
        adopt_account(&m, &AccountId::default_bitbucket(), &ana).unwrap();
        let reader = m.lock().unwrap().reader();
        let snap = read_snapshot(&reader).unwrap();
        assert_eq!(snap.prs.len(), 4, "the same account keeps its cache");
        assert_eq!(snap.account.as_deref(), Some("{ana}"));
        let bo = UserRef {
            id: "{bo}".into(),
            display_name: "Bo".into(),
            login: None,
        };
        adopt_account(&m, &AccountId::default_bitbucket(), &bo).unwrap();
        let snap = read_snapshot(&reader).unwrap();
        assert!(snap.prs.is_empty() && snap.errors.is_empty() && snap.repo_count == 0);
        assert_eq!(snap.account.as_deref(), Some("{bo}"));
        assert_eq!(load_me(&m.lock().unwrap()), Some(bo));
    }

    /// `repos` x `per_repo` PRs, every one with a diffstat, a few repo errors.
    pub(crate) fn populate(store: &Store, repos: usize, per_repo: u64) {
        let (_, demo, _) = crate::demo::demo_data();
        let template = demo[0].clone();
        let refs: Vec<RepoRef> = (0..repos)
            .map(|i| RepoRef {
                host: prcore::model::BITBUCKET_HOST.into(),
                namespace: "acme".into(),
                slug: format!("repo{i:03}"),
            })
            .collect();
        store.upsert_repos(&refs).unwrap();
        store.set_selection(&refs).unwrap();
        for (i, r) in refs.iter().enumerate() {
            let prs: Vec<PullRequest> = (1..=per_repo)
                .map(|id| {
                    let mut p = template.clone();
                    p.repo = r.clone();
                    p.id = id;
                    p.title = format!("PR {id} of {}", r.slug);
                    p.source_commit = format!("c{id}");
                    p
                })
                .collect();
            let stats: Vec<store::DiffstatRow> = prs
                .iter()
                .map(|p| {
                    let d = DiffStat {
                        files: p.id as u32,
                        lines_added: 10,
                        lines_removed: 2,
                        max_file_lines: 7,
                    };
                    (p.id, p.source_commit.clone(), d)
                })
                .collect();
            let error = (i % 10 == 0).then_some("rate limited");
            store.apply_repo_sync(r, Some(&prs), &stats, error).unwrap();
        }
    }

    /// The next message of `b` that `pick` accepts (others are skipped), within 10 s.
    fn wait_for<T>(b: &mut Backend, pick: impl Fn(Msg) -> Option<T>) -> T {
        let t = std::time::Instant::now();
        while t.elapsed() < std::time::Duration::from_secs(10) {
            match b.rx.try_recv() {
                Ok(m) => {
                    if let Some(v) = pick(m) {
                        return v;
                    }
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        panic!("no such message");
    }

    #[test]
    fn a_saved_selection_hides_unselected_repos_and_purges_on_request() {
        let (mut b, _) = backend_with_saved_login();
        populate(&b.store.lock().unwrap(), 3, 2);
        let keep = RepoRef::parse_bitbucket("acme/repo000").unwrap();
        let hide = RepoRef::parse_bitbucket("acme/repo001").unwrap();
        let purge = RepoRef::parse_bitbucket("acme/repo002").unwrap();
        b.save_selection(
            vec![(AccountId::default_bitbucket(), keep.clone())],
            vec![purge.clone()],
        );
        let saved = wait_for(&mut b, |m| match m {
            Msg::SelectionSaved(r) => Some(r),
            _ => None,
        });
        assert_eq!(saved, Ok(()));
        let snap = read_snapshot(&b.reader).unwrap();
        assert!(snap.prs.iter().all(|p| p.repo == keep));
        assert_eq!((snap.prs.len(), snap.repo_count), (2, 1));
        let cached = b.store.lock().unwrap().list_prs().unwrap();
        assert!(cached.iter().any(|p| p.repo == hide), "kept, hidden");
        assert!(!cached.iter().any(|p| p.repo == purge), "purged");
        b.request_repo_choices();
        let choices = wait_for(&mut b, |m| match m {
            Msg::RepoChoices(c) => Some(c.unwrap()),
            _ => None,
        });
        let selected: Vec<&RepoRef> = choices
            .iter()
            .filter(|c| c.selected)
            .map(|c| &c.repo)
            .collect();
        assert_eq!(selected, vec![&keep]);
    }

    #[test]
    fn discovery_errors_name_the_category_only() {
        assert_eq!(
            discovery_error(&DiscoveryError::Workspaces("network error".into())),
            "Could not list your repositories (network error)"
        );
    }

    #[test]
    fn joined_snapshot_equals_n_plus_one_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        populate(&store, 12, 4);
        // one PR without a diffstat
        let lone = RepoRef::parse_bitbucket("acme/lone").unwrap();
        let mut p = crate::demo::demo_data().1[0].clone();
        p.repo = lone.clone();
        store.replace_prs(&lone, &[p]).unwrap();
        let mut all = store.selected_repos().unwrap();
        all.push(lone);
        store.set_selection(&all).unwrap();
        let old = n_plus_one_snapshot(&store);
        let new = read_snapshot(&store.reader()).unwrap();
        assert_eq!(new, old);
        assert_eq!(new.prs.len(), 49);
        assert_eq!(new.stats.len(), 48);
        // the inbox built from both is the same
        let me = old.prs[0].author.clone();
        for view in [crate::inbox::View::All, crate::inbox::View::NeedsMe] {
            let a = crate::inbox::build_inbox(&old.prs, &old.stats, &me, view, "");
            let b = crate::inbox::build_inbox(&new.prs, &new.stats, &me, view, "");
            assert_eq!(a, b);
        }
    }

    #[test]
    fn requested_snapshots_arrive_as_messages() {
        let (mut b, _) = backend_with_saved_login();
        populate(&b.store.lock().unwrap(), 3, 2);
        for _ in 0..5 {
            b.request_snapshot();
        }
        let t = std::time::Instant::now();
        let mut last = None;
        while t.elapsed() < std::time::Duration::from_secs(10) {
            match b.rx.try_recv() {
                Ok(Msg::Snapshot(s)) => last = Some(s),
                Ok(_) => {}
                Err(_) if last.is_some() && !b.snap_busy.load(Ordering::SeqCst) => break,
                Err(_) => std::thread::yield_now(),
            }
        }
        let snap = last.expect("a snapshot arrived").unwrap();
        assert_eq!(snap.prs.len(), 6);
        assert_eq!(snap.repo_count, 3);
    }

    /// The audit's snapshot bench (section 1.5): 500 PRs in 50 repos in a file store, the
    /// snapshot alone and while two threads write diffstats through the same mutex. Run with
    /// `cargo test -p app --release -- --ignored --nocapture bench_snapshot`.
    #[test]
    #[ignore]
    fn bench_snapshot_with_writers() {
        let base = std::env::var_os("NIT_BENCH_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::tempdir_in(base).unwrap();
        let opened = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        let reader = opened.reader();
        let store = Arc::new(Mutex::new(opened));
        let t = std::time::Instant::now();
        populate(&store.lock().unwrap(), 50, 10);
        eprintln!(
            "populate 50 repos x 10 PRs (one transaction per repo): {:.1} ms in {}",
            t.elapsed().as_secs_f64() * 1000.0,
            dir.path().display()
        );
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let median = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (v[v.len() / 2], *v.last().unwrap())
        };
        let mut joined = Vec::new();
        let mut old = Vec::new();
        for _ in 0..20 {
            let t = std::time::Instant::now();
            assert_eq!(read_snapshot(&reader).unwrap().prs.len(), 500);
            joined.push(ms(t.elapsed()));
            let t = std::time::Instant::now();
            n_plus_one_snapshot(&store.lock().unwrap());
            old.push(ms(t.elapsed()));
        }
        eprintln!(
            "snapshot, joined query: median/max {:.2?} ms",
            median(joined)
        );
        eprintln!(
            "snapshot, old N+1 reads (WAL): median/max {:.2?} ms",
            median(old)
        );
        let mut per_write = Vec::new();
        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..2)
            .map(|w| {
                let (store, stop) = (store.clone(), stop.clone());
                std::thread::spawn(move || {
                    let r = RepoRef::parse_bitbucket(&format!("acme/repo{w:03}")).unwrap();
                    let mut times = Vec::new();
                    let mut i = 0u32;
                    while !stop.load(Ordering::SeqCst) {
                        i += 1;
                        let d = DiffStat {
                            files: i,
                            lines_added: i,
                            lines_removed: 0,
                            max_file_lines: i,
                        };
                        let t = std::time::Instant::now();
                        store
                            .lock()
                            .unwrap()
                            .put_diffstat(&r, (i % 10 + 1) as u64, &format!("w{i}"), &d)
                            .unwrap();
                        times.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    times
                })
            })
            .collect();
        let mut waits = Vec::new();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let t = std::time::Instant::now();
            read_snapshot(&reader).unwrap();
            waits.push(ms(t.elapsed()));
        }
        stop.store(true, Ordering::SeqCst);
        for w in writers {
            per_write.extend(w.join().unwrap());
        }
        eprintln!(
            "snapshot build (off the UI thread) with 2 writers: median/max {:.2?} ms",
            median(waits)
        );
        eprintln!(
            "diffstat write through the mutex: {} writes, median/max {:.3?} ms",
            per_write.len(),
            median(per_write)
        );
        // what the UI thread does now: ask, never read
        let secrets = Arc::new(store::MemorySecrets::new());
        let b = Backend::new(
            eframe::egui::Context::default(),
            secrets,
            Store::open(&dir.path().join("cache.sqlite")).unwrap(),
        )
        .unwrap();
        let guard = b.store.lock().unwrap();
        let t = std::time::Instant::now();
        b.request_snapshot();
        eprintln!(
            "UI thread cost of a refresh (store mutex held elsewhere): {:.3} ms",
            ms(t.elapsed())
        );
        drop(guard);
    }
}
