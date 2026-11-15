//! Metadata-first sync engine. `run_discovery` lists workspaces and repositories (a cache the
//! user picks from); `run_sync` stores PR metadata and diffstats of the selected repositories
//! only.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use prcore::model::{AccountId, PrState, PullRequest, RepoRef};
use providers::provider::{PrList, SkippedPr};
use providers::{Provider, ProviderError};
use store::Store;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    Started,
    /// The number of (selected) repositories this sync covers.
    Discovered {
        repos: usize,
    },
    RepoDone {
        repo: RepoRef,
        open: usize,
    },
    RepoFailed {
        repo: RepoRef,
        error: String,
    },
    DiffstatDone {
        repo: RepoRef,
        id: u64,
    },
    Finished,
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub concurrency: usize,
    pub fetch_diffstats: bool,
    /// The account this run syncs: only the repositories selected under it, and every row
    /// written is owned by it.
    pub account: AccountId,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            concurrency: 8,
            fetch_diffstats: true,
            account: AccountId::default_bitbucket(),
        }
    }
}

/// Repo error text for undecodable PR entries: ids and field paths only, never values.
fn skip_note(skipped: &[SkippedPr]) -> String {
    const SHOWN: usize = 3;
    let n = skipped.len();
    let mut items: Vec<String> = skipped
        .iter()
        .take(SHOWN)
        .map(|s| match s.id {
            Some(id) => format!("#{id} field: {}", s.field),
            None => format!("field: {}", s.field),
        })
        .collect();
    if n > SHOWN {
        items.push("...".into());
    }
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} PR{plural} skipped: decode ({})", items.join("; "))
}

/// Runs `f` with the store locked. The guard never outlives the call, so it is
/// never held across an `.await`.
fn with_store<T>(store: &Mutex<Store>, f: impl FnOnce(&Store) -> T) -> T {
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

struct Ctx<P: ?Sized> {
    p: Arc<P>,
    store: Arc<Mutex<Store>>,
    opts: SyncOptions,
    tx: UnboundedSender<SyncEvent>,
    sem: Semaphore,
    abort: AtomicBool,
}

impl<P: ?Sized> Ctx<P> {
    fn emit(&self, ev: SyncEvent) {
        let _ = self.tx.send(ev);
    }

    fn record_error(&self, repo: &RepoRef, msg: Option<&str>) {
        let _ = with_store(&self.store, |s| {
            s.set_repo_error_for(&self.opts.account, repo, msg)
        });
    }

    fn fail(&self, repo: RepoRef, error: String) {
        self.record_error(&repo, Some(&error));
        self.emit(SyncEvent::RepoFailed { repo, error });
    }
}

pub async fn run_sync<P: Provider + ?Sized + 'static>(
    p: Arc<P>,
    store: Arc<Mutex<Store>>,
    opts: SyncOptions,
    tx: UnboundedSender<SyncEvent>,
) -> Result<(), ProviderError> {
    let ctx = Arc::new(Ctx {
        sem: Semaphore::new(opts.concurrency.max(1)),
        p,
        store,
        opts,
        tx,
        abort: AtomicBool::new(false),
    });
    ctx.emit(SyncEvent::Started);
    let result = run_inner(&ctx).await;
    ctx.emit(SyncEvent::Finished);
    result
}

/// The error row of the whole workspace list (`*/*`); a workspace's listing uses `ws/*`.
fn pseudo_all() -> RepoRef {
    RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "*".into(),
        slug: "*".into(),
    }
}

/// What one discovery listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// Repositories listed.
    pub repos: usize,
    /// Workspaces whose repositories could not be listed (their cached entries are kept).
    pub failed_workspaces: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    Auth,
    /// The workspace list could not be read: the error category.
    Workspaces(String),
    /// The result could not be stored.
    Store,
}

/// Lists the workspaces and their repositories and stores them as the discovery cache
/// (`Store::record_discovery`). Makes no PR, diffstat or comment call and never changes the
/// selection. Listing errors are kept as before: `*/*` for the workspace list, `ws/*` for one
/// workspace's repositories.
pub async fn run_discovery<P: Provider + ?Sized>(
    p: &P,
    store: &Mutex<Store>,
) -> Result<Discovery, DiscoveryError> {
    run_discovery_for(p, store, &AccountId::default_bitbucket()).await
}

/// `run_discovery` for `account`: its choices and its listing error rows only.
pub async fn run_discovery_for<P: Provider + ?Sized>(
    p: &P,
    store: &Mutex<Store>,
    account: &AccountId,
) -> Result<Discovery, DiscoveryError> {
    let record = |repo: &RepoRef, msg: Option<&str>| {
        let _ = with_store(store, |s| s.set_repo_error_for(account, repo, msg));
    };
    let found = match p.discover().await {
        Ok(d) => {
            record(&pseudo_all(), None);
            d
        }
        Err(ProviderError::Auth) => return Err(DiscoveryError::Auth),
        Err(e) => {
            record(&pseudo_all(), Some(&e.category()));
            return Err(DiscoveryError::Workspaces(e.category()));
        }
    };
    for ws in &found.namespaces {
        let pseudo = RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: ws.clone(),
            slug: "*".into(),
        };
        let failure = found.failed.iter().find(|(f, _)| f == ws);
        record(&pseudo, failure.map(|(_, e)| e.category()).as_deref());
    }
    let listed_workspaces = found.listed_namespaces();
    let listed = found.repos;
    let failed_workspaces: Vec<String> = found.failed.into_iter().map(|(ws, _)| ws).collect();
    let workspaces = found.namespaces;
    with_store(store, |s| {
        s.record_discovery_for(
            account,
            &listed,
            &listed_workspaces,
            Some(workspaces.as_slice()),
        )
    })
    .map_err(|_| DiscoveryError::Store)?;
    Ok(Discovery {
        repos: listed.len(),
        failed_workspaces,
    })
}

/// Syncs the selected repositories only (`Store::selected_repos`); lists no workspace or
/// repository (that is `run_discovery`).
async fn run_inner<P: Provider + ?Sized + 'static>(ctx: &Arc<Ctx<P>>) -> Result<(), ProviderError> {
    let repos = match with_store(&ctx.store, |s| s.selected_repos_by_account()) {
        Ok(r) => r
            .into_iter()
            .filter(|(a, _)| *a == ctx.opts.account)
            .map(|(_, r)| r)
            .collect(),
        Err(_) => {
            ctx.record_error(&pseudo_all(), Some("store error"));
            vec![]
        }
    };
    ctx.emit(SyncEvent::Discovered { repos: repos.len() });

    let mut set = JoinSet::new();
    for repo in repos {
        let ctx = ctx.clone();
        set.spawn(async move {
            let Ok(_permit) = ctx.sem.acquire().await else {
                return;
            };
            if ctx.abort.load(Ordering::SeqCst) {
                return;
            }
            if sync_repo(&ctx, repo).await.is_err() {
                ctx.abort.store(true, Ordering::SeqCst);
            }
        });
    }
    while set.join_next().await.is_some() {}

    if ctx.abort.load(Ordering::SeqCst) {
        Err(ProviderError::Auth)
    } else {
        Ok(())
    }
}

/// Syncs one repo. `Err(())` means an Auth failure occurred and the run must abort.
async fn sync_repo<P: Provider + ?Sized>(ctx: &Ctx<P>, repo: RepoRef) -> Result<(), ()> {
    let PrList {
        prs: open,
        mut skipped,
    } = match ctx.p.prs(&repo, PrState::Open, usize::MAX).await {
        Ok(v) => v,
        Err(ProviderError::Auth) => return Err(()),
        Err(e) => {
            ctx.fail(repo, e.category());
            return Ok(());
        }
    };
    if open.is_empty() && !skipped.is_empty() {
        // Nothing usable came back: keep the cached PRs, as for a failed fetch.
        ctx.fail(repo, skip_note(&skipped));
        return Ok(());
    }
    let mut failure = None;
    let mut merged: Vec<PullRequest> = Vec::new();
    // Merged PRs matter only as the parent of an open PR whose target branch is no
    // longer the source of any open PR.
    let open_sources: BTreeSet<&str> = open.iter().map(|o| o.source_branch.as_str()).collect();
    let targets: BTreeSet<String> = open
        .iter()
        .map(|o| o.dest_branch.clone())
        .filter(|d| !open_sources.contains(d.as_str()))
        .collect();
    let mut failed_targets: BTreeSet<&String> = BTreeSet::new();
    for target in &targets {
        match ctx.p.merged_by_source_branch(&repo, target).await {
            Ok(v) => {
                skipped.extend(v.skipped);
                merged.extend(
                    v.prs
                        .into_iter()
                        .filter(|m| m.state == PrState::Merged && m.source_branch == *target),
                );
            }
            Err(ProviderError::Auth) => return Err(()),
            Err(e) => {
                failure = failure.or_else(|| Some(e.category()));
                failed_targets.insert(target);
            }
        }
    }
    let mut store_ok = true;
    if !failed_targets.is_empty() {
        // Only targets whose lookup failed fall back to what an earlier sync stored.
        match with_store(&ctx.store, |s| s.list_prs()) {
            Ok(stored) => {
                for m in stored {
                    let seen = merged.iter().any(|x| x.id == m.id);
                    if m.repo == repo
                        && m.state == PrState::Merged
                        && failed_targets.contains(&m.source_branch)
                        && !seen
                    {
                        merged.push(m);
                    }
                }
            }
            Err(_) => {
                // Cannot tell what was stored: leave the stored list untouched.
                store_ok = false;
                failure = Some("store error".to_string());
            }
        }
    }

    let all = store_ok.then(|| {
        let mut all = open.clone();
        all.extend(merged);
        // the PR is seen through this account ("me" and posting go through it)
        for pr in &mut all {
            pr.account = ctx.opts.account.clone();
        }
        all
    });

    let notes = |failure: Option<String>| {
        let mut notes: Vec<String> = failure.into_iter().collect();
        if !skipped.is_empty() {
            notes.push(skip_note(&skipped));
        }
        (!notes.is_empty()).then(|| notes.join("; "))
    };
    let error = notes(failure);
    // First write: the PR list and the error row in one transaction, so the inbox shows the
    // repo's PRs before (and even without) its diffstats.
    let stored = with_store(&ctx.store, |s| {
        s.apply_repo_sync_for(
            &ctx.opts.account,
            &repo,
            all.as_deref(),
            &[],
            error.as_deref(),
        )
    });
    match (stored, error) {
        (Ok(()), None) => ctx.emit(SyncEvent::RepoDone {
            repo: repo.clone(),
            open: open.len(),
        }),
        (Ok(()), Some(error)) => ctx.emit(SyncEvent::RepoFailed {
            repo: repo.clone(),
            error,
        }),
        (Err(_), _) => {
            let error = notes(Some("store error".to_string())).unwrap_or_default();
            ctx.fail(repo.clone(), error);
        }
    }

    // Then the diffstats of open PRs whose source commit changed, in a second transaction.
    let mut stats = Vec::new();
    let mut auth_failed = false;
    if ctx.opts.fetch_diffstats {
        // unreadable: every diffstat counts as stale
        let known = with_store(&ctx.store, |s| s.diffstat_commits(&repo)).unwrap_or_default();
        for pr in &open {
            if known.get(&pr.id) == Some(&pr.source_commit) {
                continue;
            }
            match ctx.p.diffstat(&repo, pr.id).await {
                Ok(stat) => stats.push((pr.id, pr.source_commit.clone(), stat)),
                Err(ProviderError::Auth) => {
                    // what was fetched so far is still stored; the run then aborts
                    auth_failed = true;
                    break;
                }
                Err(_) => {}
            }
        }
    }
    if !stats.is_empty()
        && with_store(&ctx.store, |s| {
            s.put_diffstats_for(&ctx.opts.account, &repo, &stats)
        })
        .is_ok()
    {
        for (id, _, _) in &stats {
            ctx.emit(SyncEvent::DiffstatDone {
                repo: repo.clone(),
                id: *id,
            });
        }
    }
    if auth_failed {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
