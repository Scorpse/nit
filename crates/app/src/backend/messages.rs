use super::*;
use prcore::commit::same_commit;
use prcore::history::{caught_up_key, CAUGHT_UP_PREFIX};

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

/// Builds snapshots while `again` is set, each sent into the channel `tx` holds at the time
/// it is sent (not the one of the request that started the loop), then clears `busy`.
pub(super) fn snapshot_builds(
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
    // an unreadable marker only loses the "new commits" flags
    let caught_up = reader.meta_prefix(CAUGHT_UP_PREFIX).unwrap_or_default();
    let mut prs = Vec::with_capacity(c.prs.len());
    let mut stats = HashMap::new();
    for (mut p, stat) in c.prs {
        p.new_commits = !p.source_commit.is_empty()
            && caught_up
                .get(&caught_up_key(&p.repo, p.id))
                .is_some_and(|head| {
                    *head != p.source_commit && !same_commit(head, &p.source_commit)
                });
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
