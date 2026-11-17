use super::*;

/// User-facing text of a failed discovery (categories only, never detail).
pub(super) fn discovery_error(e: &DiscoveryError) -> String {
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
pub(super) async fn discover_and_report(
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
pub(super) fn sync_targets(store: &Mutex<Store>, with_default: bool) -> Vec<AccountId> {
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
pub(super) fn record_account_error(store: &Mutex<Store>, account: &AccountId, msg: &str) {
    let pseudo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "*".into(),
        slug: "*".into(),
    };
    let _ = lock_ok(store).set_repo_error_for(account, &pseudo, Some(msg));
}

/// Per-account sync concurrency (spec 4.3): Bitbucket 8, GitHub and GitLab 4.
pub(super) fn concurrency_for(forge: Forge) -> usize {
    match forge {
        Forge::BitbucketCloud => 8,
        Forge::GitHub | Forge::GitLab => 4,
    }
}

/// Syncs `accounts` one after the other, each with its own provider and options, as one
/// sync for the UI: one `Started`, the repository count summed, one `Finished`. A failing
/// account (no credentials, 401, ...) is recorded on its own rows and the next one still
/// syncs. Returns the default account's result (its 401 asks for a new sign-in).
pub(super) async fn sync_accounts(
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
pub(super) fn record_account_clear(store: &Mutex<Store>, account: &AccountId) {
    let pseudo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "*".into(),
        slug: "*".into(),
    };
    let _ = lock_ok(store).set_repo_error_for(account, &pseudo, None);
}

/// The accounts whose client is paused by a rate limit right now, with until when.
pub(super) fn paused_accounts(reg: &ProviderRegistry) -> Vec<(AccountId, std::time::SystemTime)> {
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
pub(super) fn requested_elsewhere(store: &Store, found: &[RepoRef]) -> Vec<(RepoRef, usize)> {
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
pub(super) async fn report_requested_elsewhere(
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
    jiff::Timestamp::try_from(t).map_or_else(
        |_| String::new(),
        |t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M")
                .to_string()
        },
    )
}
