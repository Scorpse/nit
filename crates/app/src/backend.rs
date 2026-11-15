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

mod accounts;
mod messages;
mod poster;
mod registry;
mod sync_driver;

pub use accounts::*;
pub use messages::*;
use poster::*;
pub(crate) use registry::*;
pub use sync_driver::*;

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

#[cfg(test)]
mod leak_check;

#[cfg(test)]
pub(crate) mod tests;
