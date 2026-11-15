use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use prcore::comment_type::CommentType;
use prcore::comments::ThreadId;
use prcore::draft::{Draft, DraftKind, Side};
use prcore::model::{
    AccountId, DiffStat, Forge, PullRequest, RepoInfo, RepoRef, Reviewer, BITBUCKET_HOST,
    DEFAULT_BITBUCKET_ACCOUNT,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

/// Schema version this build understands (stored in `PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 8;

const SCHEMA_V1: &str = "
CREATE TABLE repos (
    full_name TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    slug TEXT NOT NULL
);
CREATE TABLE prs (
    repo TEXT NOT NULL,
    id INTEGER NOT NULL,
    state TEXT NOT NULL,
    updated_on TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE INDEX prs_state ON prs (state);
CREATE INDEX prs_updated_on ON prs (updated_on);
CREATE TABLE diffstats (
    repo TEXT NOT NULL,
    id INTEGER NOT NULL,
    source_commit TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE TABLE repo_errors (
    full_name TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    slug TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const SCHEMA_V2: &str = "
CREATE TABLE viewed (
    repo TEXT NOT NULL,
    pr_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    oid TEXT NOT NULL,
    PRIMARY KEY (repo, pr_id, path)
);
CREATE TABLE drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    pr_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    path TEXT,
    line INTEGER,
    side TEXT,
    parent_id INTEGER,
    text TEXT NOT NULL,
    source_commit TEXT NOT NULL
);
CREATE INDEX drafts_pr ON drafts (repo, pr_id);
";

/// v3: a draft the server accepted but that is not deleted yet is tombstoned, never posted
/// again.
const SCHEMA_V3: &str = "ALTER TABLE drafts ADD COLUMN posted_at TEXT;";

/// v4: set right before a draft is sent; a draft with it and without `posted_at` has an
/// unknown outcome (a crash or a lost answer) and is never offered for posting silently.
const SCHEMA_V4: &str = "ALTER TABLE drafts ADD COLUMN posting_started_at TEXT;";

/// v5: the repositories a discovery listed (a cache) and the ones the user chose to sync.
/// `gone`: a later discovery of its workspace no longer listed it (kept only while selected).
/// A cache that already holds PRs keeps syncing their repositories: they are selected once,
/// here, and the count is left in meta for a one-time notice.
const SCHEMA_V5: &str = "
CREATE TABLE repo_choices (
    full_name TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    slug TEXT NOT NULL,
    name TEXT,
    is_private INTEGER,
    selected INTEGER NOT NULL DEFAULT 0,
    discovered_at TEXT,
    gone INTEGER NOT NULL DEFAULT 0
);
INSERT INTO repo_choices (full_name, workspace, slug, selected)
    SELECT DISTINCT repo, substr(repo, 1, instr(repo, '/') - 1), substr(repo, instr(repo, '/') + 1), 1
    FROM prs WHERE instr(repo, '/') > 1;
INSERT INTO meta (key, value)
    SELECT 'repos_preselected', COUNT(*) FROM repo_choices HAVING COUNT(*) > 0;
";

/// v6: the draft's optional comment type ("nit", "issue", ...); NULL for none and for every
/// draft written before.
const SCHEMA_V6: &str = "ALTER TABLE drafts ADD COLUMN comment_type TEXT;";

/// v7 (multi-forge): accounts, sync cursors, and host-qualified repo keys
/// (`bitbucket.org/ws/slug`, lower-case: `RepoRef::key`) in every repo column, renamed
/// `repo_key` (and `workspace` to `namespace`). Account-owned rows gain `account_id`; every
/// existing row belongs to the single Bitbucket sign-in, `DEFAULT_BITBUCKET_ACCOUNT`. Drafts
/// gain `reply_to` (the thread id as text; `parent_id` is kept, unused) and `forge`.
/// `repo_choices` is rebuilt to key on `(account_id, repo_key)`. The key rewrite is done by
/// `migrate_v7` in Rust, in the same transaction.
const SCHEMA_V7: &str = "
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    forge TEXT NOT NULL,
    web_base TEXT NOT NULL,
    api_base TEXT NOT NULL,
    graphql_base TEXT,
    git_host TEXT NOT NULL,
    user_id TEXT NOT NULL DEFAULT '',
    login TEXT NOT NULL DEFAULT '',
    display_name TEXT NOT NULL DEFAULT '',
    auth_kind TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_ok_at TEXT,
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE sync_cursors (
    account_id TEXT NOT NULL,
    repo_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    etag TEXT,
    last_updated TEXT,
    checked_at TEXT NOT NULL,
    PRIMARY KEY (account_id, repo_key, kind)
);
ALTER TABLE repos RENAME COLUMN full_name TO repo_key;
ALTER TABLE repos RENAME COLUMN workspace TO namespace;
ALTER TABLE repos ADD COLUMN host TEXT NOT NULL DEFAULT 'bitbucket.org';
ALTER TABLE repos ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE prs RENAME COLUMN repo TO repo_key;
ALTER TABLE prs ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE diffstats RENAME COLUMN repo TO repo_key;
ALTER TABLE diffstats ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE repo_errors RENAME COLUMN full_name TO repo_key;
ALTER TABLE repo_errors RENAME COLUMN workspace TO namespace;
ALTER TABLE repo_errors ADD COLUMN host TEXT NOT NULL DEFAULT 'bitbucket.org';
ALTER TABLE repo_errors ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE viewed RENAME COLUMN repo TO repo_key;
ALTER TABLE viewed ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE drafts RENAME COLUMN repo TO repo_key;
ALTER TABLE drafts ADD COLUMN account_id TEXT NOT NULL DEFAULT 'acc-bitbucket-default';
ALTER TABLE drafts ADD COLUMN forge TEXT NOT NULL DEFAULT 'bitbucket';
ALTER TABLE drafts ADD COLUMN reply_to TEXT;
UPDATE drafts SET reply_to = CAST(parent_id AS TEXT) WHERE parent_id IS NOT NULL;
CREATE TABLE repo_choices_v7 (
    account_id TEXT NOT NULL,
    repo_key TEXT NOT NULL,
    host TEXT NOT NULL,
    namespace TEXT NOT NULL,
    slug TEXT NOT NULL,
    name TEXT,
    is_private INTEGER,
    selected INTEGER NOT NULL DEFAULT 0,
    discovered_at TEXT,
    gone INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, repo_key)
);
";

/// v8: the first line of a multi-line draft and its side ("old" / "new"); NULL for one line
/// and for every draft written before.
const SCHEMA_V8: &str = "
ALTER TABLE drafts ADD COLUMN start_side TEXT;
ALTER TABLE drafts ADD COLUMN start_line INTEGER;
";

const META_CLONE_ROOT: &str = "clone_root";
const META_DOWNLOAD_DIR: &str = "download_dir";
/// The user the default account was last signed in as (see `Store::adopt_user`).
const META_ACCOUNT_ID: &str = "account_id";
/// How many repositories the v5 migration selected because the cache already held their PRs;
/// deleted when the user dismisses the notice about it.
pub const META_REPOS_PRESELECTED: &str = "repos_preselected";

/// One repository of the discovery cache and whether the user chose to sync it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoChoice {
    pub repo: RepoRef,
    pub name: Option<String>,
    pub private: Option<bool>,
    pub selected: bool,
    /// When a discovery last listed it (UTC, `YYYY-MM-DD HH:MM:SS`); `None` before any did.
    pub discovered_at: Option<String>,
    /// A later discovery no longer listed it; kept (and no longer synced) only while selected.
    pub gone: bool,
    /// The account that listed it (and syncs it when selected).
    pub account: AccountId,
}

/// One signed-in account: where it lives and who it is. Holds no secret (tokens live in the
/// credential store under keys derived from `id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub forge: Forge,
    /// `https://host[/prefix]`, no trailing slash.
    pub web_base: String,
    pub api_base: String,
    pub graphql_base: Option<String>,
    /// The only host git credentials of this account are sent to.
    pub git_host: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    /// How the account signs in ("api_token" for Bitbucket, "pat" later).
    pub auth_kind: String,
    pub enabled: bool,
}

impl Account {
    /// The single Bitbucket Cloud sign-in every pre-multi-forge cache belongs to.
    pub fn default_bitbucket(user_id: String, display_name: String) -> Account {
        Account {
            id: AccountId::default_bitbucket(),
            forge: Forge::BitbucketCloud,
            web_base: "https://bitbucket.org".into(),
            api_base: "https://api.bitbucket.org/2.0".into(),
            graphql_base: None,
            git_host: BITBUCKET_HOST.into(),
            user_id,
            login: String::new(),
            display_name,
            auth_kind: "api_token".into(),
            enabled: true,
        }
    }
}

/// What `Store::remove_account` deleted, and the drafts it kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemovedCounts {
    pub prs: usize,
    pub diffstats: usize,
    pub repo_choices: usize,
    pub repo_errors: usize,
    pub cursors: usize,
    /// Drafts of the account stay (shown as "account removed", never posted).
    pub drafts_kept: usize,
}

/// A sync position of one repository: the list request's ETag and the newest `updated_at`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncCursorRow {
    pub etag: Option<String>,
    pub last_updated: Option<String>,
}

/// User-chosen folders. `download_dir` falls back to a caller-supplied default when unset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub clone_root: Option<PathBuf>,
    pub download_dir: PathBuf,
}

fn kind_str(k: DraftKind) -> &'static str {
    match k {
        DraftKind::Pr => "pr",
        DraftKind::File => "file",
        DraftKind::Inline => "inline",
        DraftKind::Reply => "reply",
    }
}

fn parse_kind(s: &str) -> Option<DraftKind> {
    Some(match s {
        "pr" => DraftKind::Pr,
        "file" => DraftKind::File,
        "inline" => DraftKind::Inline,
        "reply" => DraftKind::Reply,
        _ => return None,
    })
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn parse_side(s: &str) -> Option<Side> {
    match s {
        "old" => Some(Side::Old),
        "new" => Some(Side::New),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored data could not be serialized or deserialized")]
    Json(#[from] serde_json::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("stored data is malformed")]
    Corrupt,
    #[error("record not found")]
    NotFound,
    #[error(
        "The cache folder does not support WAL mode (network drive?); the cache uses a slower \
         rollback journal instead"
    )]
    WalUnavailable,
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A diffstat as the sync stores it: PR id, source commit it was computed for, the stat.
pub type DiffstatRow = (u64, String, DiffStat);

/// What the inbox needs, read in one read transaction (one joined query for PRs and their
/// diffstats).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CacheSnapshot {
    /// Every stored PR of a selected repository (ordered by repo and id) with its diffstat and
    /// the commit it is for. PRs of unselected repositories stay cached but are left out.
    pub prs: Vec<(PullRequest, Option<(String, DiffStat)>)>,
    /// Errors of selected repositories and of workspace listings (slug `*`).
    pub errors: Vec<(RepoRef, String)>,
    /// Selected repositories that are still accessible (what a sync covers).
    pub repo_count: usize,
    /// Repositories in the discovery cache that are still accessible.
    pub known_repos: usize,
    /// `META_REPOS_PRESELECTED`: repositories selected by the upgrade, until dismissed.
    pub preselected: Option<usize>,
    /// The user the default account was last signed in as (`adopt_user`); `None` before the first
    /// sign-in that recorded one.
    pub account_id: Option<String>,
    /// Every account's own user id (accounts that recorded one), for per-account "Mine"
    /// and "Needs me".
    pub account_users: Vec<(AccountId, String)>,
}

type Conn = Arc<Mutex<Connection>>;

/// The cache file, with one connection per durability class (R-store):
///
/// - `safe` (`synchronous=FULL`): drafts, posting markers, settings and other meta. A commit
///   here is on disk when it returns, so a "posting" marker or a tombstone survives a power
///   loss.
/// - `fast` (`synchronous=NORMAL`): the rebuildable tables (`prs`, `diffstats`, `repos`,
///   `repo_errors`, `viewed`). In WAL mode a power loss can drop the last commits, never
///   corrupt the file; a sync puts them back.
/// - `read`: inbox and cache reads (WAL readers never wait for a writer).
///
/// The file is in WAL mode. Where WAL is unavailable (some network drives) the file uses a
/// rollback journal (`journal_mode=DELETE`) and both writing connections use
/// `synchronous=FULL`, since NORMAL is only corruption-safe in WAL mode. An in-memory store has
/// a single connection behind all three. No method holds two of the connections at once.
pub struct Store {
    safe: Conn,
    fast: Conn,
    read: Conn,
    /// WAL could not be enabled; the rollback-journal fallback is in use.
    no_wal: bool,
}

fn lock(c: &Conn) -> MutexGuard<'_, Connection> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

/// A write transaction that takes the write lock up front (`BEGIN IMMEDIATE`), so a second
/// writer connection waits in the busy handler instead of failing when it upgrades a read.
fn write_tx(conn: &Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
}

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Every statement of the store fits, also on the single in-memory connection.
const STATEMENT_CACHE: usize = 64;

/// Switches `conn` to WAL; `WalUnavailable` when the file system does not allow it.
fn enable_wal(conn: &Connection) -> StoreResult<()> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
        .map_err(|_| StoreError::WalUnavailable)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::WalUnavailable);
    }
    Ok(())
}

/// How the connections of one file journal: WAL, or the rollback-journal fallback.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Journal {
    Wal,
    Delete,
}

fn open_file(
    path: &Path,
    journal: Journal,
    synchronous: &str,
    wal: fn(&Connection) -> StoreResult<()>,
) -> StoreResult<Connection> {
    let conn = Connection::open(path)?;
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
    conn.busy_timeout(BUSY_TIMEOUT)?;
    match journal {
        Journal::Wal => wal(&conn)?,
        Journal::Delete => {
            let _: String =
                conn.pragma_update_and_check(None, "journal_mode", "DELETE", |r| r.get(0))?;
        }
    }
    conn.pragma_update(None, "synchronous", synchronous)?;
    Ok(conn)
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Store> {
        Store::open_with(path, enable_wal)
    }

    /// `open` with the WAL switch replaceable (tests inject a failing one).
    pub(crate) fn open_with(
        path: &Path,
        wal: fn(&Connection) -> StoreResult<()>,
    ) -> StoreResult<Store> {
        match Store::open_journal(path, Journal::Wal, wal) {
            Err(StoreError::WalUnavailable) => Store::open_journal(path, Journal::Delete, wal),
            other => other,
        }
    }

    fn open_journal(
        path: &Path,
        journal: Journal,
        wal: fn(&Connection) -> StoreResult<()>,
    ) -> StoreResult<Store> {
        // without WAL, NORMAL is not corruption-safe: every writer is FULL
        let cache_sync = match journal {
            Journal::Wal => "NORMAL",
            Journal::Delete => "FULL",
        };
        let safe = open_file(path, journal, "FULL", wal)?;
        migrate(&safe)?;
        let fast = open_file(path, journal, cache_sync, wal)?;
        let read = open_file(path, journal, cache_sync, wal)?;
        read.pragma_update(None, "query_only", true)?;
        Ok(Store {
            safe: Arc::new(Mutex::new(safe)),
            fast: Arc::new(Mutex::new(fast)),
            read: Arc::new(Mutex::new(read)),
            no_wal: journal == Journal::Delete,
        })
    }

    /// True when the cache folder does not support WAL and the slower rollback journal is
    /// in use (`StoreError::WalUnavailable` says why, for a note in the UI).
    pub fn wal_unavailable(&self) -> bool {
        self.no_wal
    }

    pub fn open_in_memory() -> StoreResult<Store> {
        let conn = Connection::open_in_memory()?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
        migrate(&conn)?;
        let one = Arc::new(Mutex::new(conn));
        Ok(Store {
            safe: one.clone(),
            fast: one.clone(),
            read: one,
            no_wal: false,
        })
    }

    /// Best effort: moves the WAL into the main file and truncates it (clean shutdown).
    pub fn checkpoint(&self) -> StoreResult<()> {
        lock(&self.fast).query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    pub fn upsert_repos(&self, repos: &[RepoRef]) -> StoreResult<()> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        {
            // unchanged rows are not rewritten
            let mut stmt = tx.prepare_cached(
                "INSERT INTO repos (repo_key, namespace, slug, host) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo_key) DO UPDATE SET namespace = ?2, slug = ?3, host = ?4
                 WHERE namespace IS NOT ?2 OR slug IS NOT ?3 OR host IS NOT ?4",
            )?;
            for r in repos {
                stmt.execute(params![key_of(r), r.namespace, r.slug, r.host])?;
            }
        }
        Ok(tx.commit()?)
    }

    pub fn list_repos(&self) -> StoreResult<Vec<RepoRef>> {
        let conn = lock(&self.read);
        let mut stmt = conn.prepare_cached(
            "SELECT host, namespace, slug FROM repos ORDER BY namespace || '/' || slug",
        )?;
        let rows = stmt.query_map([], repo_at(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replaces every stored PR (any state) for `repo` in a single transaction. Nothing is
    /// written when the stored rows already equal `prs`.
    pub fn replace_prs(&self, repo: &RepoRef, prs: &[PullRequest]) -> StoreResult<()> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        replace_prs_in(&tx, DEFAULT_BITBUCKET_ACCOUNT, repo, prs)?;
        Ok(tx.commit()?)
    }

    /// Everything one repository's sync produced, in one transaction: its diffstats, its
    /// error row (`None` clears it) and, when `prs` is given, its PR list (as
    /// `replace_prs`). All of it is stored, or none. Unchanged rows are not rewritten, so a
    /// sync that found nothing new writes (almost) nothing.
    pub fn apply_repo_sync(
        &self,
        repo: &RepoRef,
        prs: Option<&[PullRequest]>,
        diffstats: &[DiffstatRow],
        error: Option<&str>,
    ) -> StoreResult<()> {
        self.apply_repo_sync_for(&AccountId::default_bitbucket(), repo, prs, diffstats, error)
    }

    /// `apply_repo_sync` for a repository `account` syncs: every row written is owned by it
    /// (a row of the same repository owned by another account is replaced).
    pub fn apply_repo_sync_for(
        &self,
        account: &AccountId,
        repo: &RepoRef,
        prs: Option<&[PullRequest]>,
        diffstats: &[DiffstatRow],
        error: Option<&str>,
    ) -> StoreResult<()> {
        let acc = account.as_str();
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        for (id, commit, stat) in diffstats {
            put_diffstat_in(&tx, acc, repo, *id, commit, stat)?;
        }
        set_repo_error_in(&tx, acc, repo, error)?;
        if let Some(prs) = prs {
            replace_prs_in(&tx, acc, repo, prs)?;
        }
        Ok(tx.commit()?)
    }

    /// Diffstats of one repository in one transaction (the sync's second write, after its PR
    /// list); unchanged rows are not rewritten.
    pub fn put_diffstats(&self, repo: &RepoRef, diffstats: &[DiffstatRow]) -> StoreResult<()> {
        self.put_diffstats_for(&AccountId::default_bitbucket(), repo, diffstats)
    }

    /// `put_diffstats` owned by `account`.
    pub fn put_diffstats_for(
        &self,
        account: &AccountId,
        repo: &RepoRef,
        diffstats: &[DiffstatRow],
    ) -> StoreResult<()> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        for (id, commit, stat) in diffstats {
            put_diffstat_in(&tx, account.as_str(), repo, *id, commit, stat)?;
        }
        Ok(tx.commit()?)
    }

    /// Records that `account` is signed in as `user_id` (a non-secret forge user id). When
    /// the account was signed in as another user before, or as no recorded one, the
    /// account's own rebuildable rows (`prs`, `diffstats`, `repo_errors`, `repos`, `viewed`)
    /// are deleted first, so nothing of the previous user stays readable; returns whether
    /// they were. Other accounts' rows are never touched (this replaces the old whole-cache
    /// wipe on an account switch).
    ///
    /// Drafts are kept (the user's text). The account's repository choices and sync cursors
    /// go too when another user was recorded; they are kept when none was (a cache from
    /// before users were recorded, whose choices the upgrade made from its PRs).
    ///
    /// For the default Bitbucket account the user is also kept in meta `account_id`, which
    /// older caches used as the owner and the inbox snapshot still reports.
    pub fn adopt_user(
        &self,
        account: &AccountId,
        user_id: &str,
        display_name: &str,
    ) -> StoreResult<bool> {
        let acc = account.as_str();
        let is_default = *account == AccountId::default_bitbucket();
        let row = self.accounts()?.into_iter().find(|a| a.id == *account);
        let previous = match row.as_ref().map(|a| a.user_id.clone()) {
            Some(u) if !u.is_empty() => Some(u),
            _ if is_default => get_meta_in(&lock(&self.safe), META_ACCOUNT_ID)?,
            _ => None,
        };
        let same = previous.as_deref() == Some(user_id);
        if !same {
            let conn = lock(&self.fast);
            let tx = write_tx(&conn)?;
            for table in ["prs", "diffstats", "repo_errors", "repos", "viewed"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE account_id = ?1"),
                    params![acc],
                )?;
            }
            tx.commit()?;
        }
        // recorded after the clear: a crash in between clears again at the next sign-in
        let conn = lock(&self.safe);
        let tx = write_tx(&conn)?;
        if !same && previous.is_some() {
            tx.prepare_cached("DELETE FROM repo_choices WHERE account_id = ?1")?
                .execute(params![acc])?;
            tx.prepare_cached("DELETE FROM sync_cursors WHERE account_id = ?1")?
                .execute(params![acc])?;
            if is_default {
                delete_meta_in(&tx, META_REPOS_PRESELECTED)?;
            }
        }
        let mut a = row.unwrap_or_else(|| Account::default_bitbucket(String::new(), String::new()));
        if a.id != *account {
            a.id = account.clone();
        }
        a.user_id = user_id.to_string();
        a.display_name = display_name.to_string();
        upsert_account_in(&tx, &a)?;
        if is_default {
            set_meta_in(&tx, META_ACCOUNT_ID, user_id)?;
        }
        tx.commit()?;
        Ok(!same)
    }

    /// Replaces the reviewers of one cached PR (after a review action of mine went through),
    /// so the inbox shows it before the next sync. False when the PR is not cached.
    pub fn set_pr_reviewers(
        &self,
        repo: &RepoRef,
        id: u64,
        reviewers: &[Reviewer],
    ) -> StoreResult<bool> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        let json: Option<String> = tx
            .prepare_cached("SELECT json FROM prs WHERE repo_key = ?1 AND id = ?2")?
            .query_row(params![key_of(repo), id as i64], |r| r.get(0))
            .optional()?;
        let Some(json) = json else {
            return Ok(false);
        };
        let mut pr: PullRequest = serde_json::from_str(&json)?;
        pr.reviewers = reviewers.to_vec();
        tx.prepare_cached("UPDATE prs SET json = ?1 WHERE repo_key = ?2 AND id = ?3")?
            .execute(params![
                serde_json::to_string(&pr)?,
                key_of(repo),
                id as i64
            ])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn list_prs(&self) -> StoreResult<Vec<PullRequest>> {
        let conn = lock(&self.read);
        let mut stmt = conn.prepare_cached("SELECT json FROM prs ORDER BY repo_key, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// The inbox's data in one read transaction: PRs joined with their diffstats, the repo
    /// errors and the repo count. An undecodable PR row is an error (the caller says the
    /// cache could not be read); an undecodable diffstat only loses that diffstat.
    pub fn cache_snapshot(&self) -> StoreResult<CacheSnapshot> {
        cache_snapshot_in(&self.read)
    }

    /// A handle on the read connection that works without whatever lock guards this
    /// `Store` (in WAL mode a reader never waits for a writer).
    pub fn reader(&self) -> StoreReader {
        StoreReader {
            read: self.read.clone(),
        }
    }
}

/// Reads of the inbox data through the store's read connection (see `Store::reader`).
#[derive(Clone)]
pub struct StoreReader {
    read: Conn,
}

impl StoreReader {
    /// `Store::cache_snapshot`.
    pub fn cache_snapshot(&self) -> StoreResult<CacheSnapshot> {
        cache_snapshot_in(&self.read)
    }
}

fn cache_snapshot_in(read: &Conn) -> StoreResult<CacheSnapshot> {
    {
        let conn = lock(read);
        // deferred: a read transaction, one consistent view of the three queries
        let tx = conn.unchecked_transaction()?;
        let mut prs = Vec::new();
        {
            let mut stmt = tx.prepare_cached(
                "SELECT p.json, d.source_commit, d.json FROM prs p
                 LEFT JOIN diffstats d ON d.repo_key = p.repo_key AND d.id = p.id
                 WHERE (p.account_id, p.repo_key) IN
                     (SELECT account_id, repo_key FROM repo_choices WHERE selected = 1)
                 ORDER BY p.repo_key, p.id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let pr: PullRequest = serde_json::from_str(&r.get::<_, String>(0)?)?;
                let commit: Option<String> = r.get(1)?;
                let stat: Option<String> = r.get(2)?;
                let stat = commit
                    .zip(stat)
                    .and_then(|(c, s)| serde_json::from_str(&s).ok().map(|s| (c, s)));
                prs.push((pr, stat));
            }
        }
        let errors = {
            let mut stmt = tx.prepare_cached(
                "SELECT host, namespace, slug, message FROM repo_errors
                 WHERE slug = '*'
                    OR (account_id, repo_key) IN
                        (SELECT account_id, repo_key FROM repo_choices WHERE selected = 1)
                 ORDER BY namespace || '/' || slug",
            )?;
            let rows = stmt.query_map([], |r| Ok((repo_at(0)(r)?, r.get(3)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let (repo_count, known_repos): (i64, i64) = tx
            .prepare_cached(
                "SELECT COALESCE(SUM(selected * (1 - gone)), 0), COALESCE(SUM(1 - gone), 0)
                 FROM repo_choices",
            )?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let preselected = get_meta_in(&tx, META_REPOS_PRESELECTED)?.and_then(|v| v.parse().ok());
        let account_id = get_meta_in(&tx, META_ACCOUNT_ID)?;
        let account_users = {
            let mut stmt = tx.prepare_cached(
                "SELECT id, user_id FROM accounts WHERE user_id != '' ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| Ok((AccountId(r.get(0)?), r.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        Ok(CacheSnapshot {
            prs,
            errors,
            repo_count: repo_count.max(0) as usize,
            known_repos: known_repos.max(0) as usize,
            preselected,
            account_id,
            account_users,
        })
    }
}

impl Store {
    pub fn put_diffstat(
        &self,
        repo: &RepoRef,
        id: u64,
        source_commit: &str,
        stat: &DiffStat,
    ) -> StoreResult<()> {
        put_diffstat_in(
            &lock(&self.fast),
            DEFAULT_BITBUCKET_ACCOUNT,
            repo,
            id,
            source_commit,
            stat,
        )
    }

    pub fn get_diffstat(&self, repo: &RepoRef, id: u64) -> StoreResult<Option<(String, DiffStat)>> {
        let row: Option<(String, String)> = lock(&self.read)
            .prepare_cached(
                "SELECT source_commit, json FROM diffstats WHERE repo_key = ?1 AND id = ?2",
            )?
            .query_row(params![key_of(repo), id as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match row {
            Some((commit, json)) => Ok(Some((commit, serde_json::from_str(&json)?))),
            None => Ok(None),
        }
    }

    /// PR id -> source commit of every stored diffstat of `repo` (one query instead of one
    /// `get_diffstat` per PR).
    pub fn diffstat_commits(&self, repo: &RepoRef) -> StoreResult<HashMap<u64, String>> {
        let conn = lock(&self.read);
        let mut stmt =
            conn.prepare_cached("SELECT id, source_commit FROM diffstats WHERE repo_key = ?1")?;
        let rows = stmt.query_map(params![key_of(repo)], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Sets the last sync error for `repo`, or clears it when `message` is `None`.
    pub fn set_repo_error(&self, repo: &RepoRef, message: Option<&str>) -> StoreResult<()> {
        self.set_repo_error_for(&AccountId::default_bitbucket(), repo, message)
    }

    /// `set_repo_error` owned by `account`; a listing pseudo repository (slug `*`) is that
    /// account's own row.
    pub fn set_repo_error_for(
        &self,
        account: &AccountId,
        repo: &RepoRef,
        message: Option<&str>,
    ) -> StoreResult<()> {
        set_repo_error_in(&lock(&self.fast), account.as_str(), repo, message)
    }

    pub fn repo_errors(&self) -> StoreResult<Vec<(RepoRef, String)>> {
        repo_errors_in(&lock(&self.read))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> StoreResult<()> {
        set_meta_in(&lock(&self.safe), key, value)
    }

    /// Removes `key`; a missing key is not an error.
    pub fn delete_meta(&self, key: &str) -> StoreResult<()> {
        delete_meta_in(&lock(&self.safe), key)
    }

    pub fn get_meta(&self, key: &str) -> StoreResult<Option<String>> {
        get_meta_in(&lock(&self.safe), key)
    }

    pub fn settings(&self, default_download: &Path) -> StoreResult<Settings> {
        let conn = lock(&self.safe);
        Ok(Settings {
            clone_root: get_meta_in(&conn, META_CLONE_ROOT)?.map(PathBuf::from),
            download_dir: get_meta_in(&conn, META_DOWNLOAD_DIR)?
                .map(PathBuf::from)
                .unwrap_or_else(|| default_download.to_path_buf()),
        })
    }

    /// Both folders in one transaction.
    pub fn save_settings(&self, s: &Settings) -> StoreResult<()> {
        let conn = lock(&self.safe);
        let tx = write_tx(&conn)?;
        match &s.clone_root {
            Some(p) => set_meta_in(&tx, META_CLONE_ROOT, &p.to_string_lossy())?,
            None => delete_meta_in(&tx, META_CLONE_ROOT)?,
        }
        set_meta_in(&tx, META_DOWNLOAD_DIR, &s.download_dir.to_string_lossy())?;
        Ok(tx.commit()?)
    }

    /// Marks `path` viewed at `oid`, or clears the mark when `viewed` is false.
    pub fn set_viewed(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        path: &str,
        oid: &str,
        viewed: bool,
    ) -> StoreResult<()> {
        set_viewed_in(&lock(&self.fast), repo, pr_id, path, oid, viewed)
    }

    /// `set_viewed` for many files of one PR in a single transaction (a directory's
    /// "viewed" box): all of them are stored, or none.
    pub fn set_viewed_many(
        &self,
        repo: &RepoRef,
        pr_id: u64,
        marks: &[(&str, &str, bool)],
    ) -> StoreResult<()> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        for &(path, oid, viewed) in marks {
            set_viewed_in(&tx, repo, pr_id, path, oid, viewed)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Viewed files of a PR: path -> oid at the time it was marked viewed.
    pub fn viewed_map(&self, repo: &RepoRef, pr_id: u64) -> StoreResult<HashMap<String, String>> {
        let conn = lock(&self.read);
        let mut stmt =
            conn.prepare_cached("SELECT path, oid FROM viewed WHERE repo_key = ?1 AND pr_id = ?2")?;
        let rows = stmt.query_map(params![key_of(repo), pr_id as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts the draft when `d.id <= 0`. Otherwise updates the row with that id, which must
    /// belong to the same repo and PR; if no such row exists (for example it was deleted) this
    /// returns `NotFound` rather than recreating it. Returns the row id.
    pub fn save_draft(&self, d: &Draft) -> StoreResult<i64> {
        let conn = lock(&self.safe);
        let repo = key_of(&d.repo);
        let line = d.line;
        let side = d.side.map(side_str);
        let parent = d.reply_to.as_ref().map(|t| t.as_str());
        let ctype = d.comment_type.map(CommentType::as_str);
        let (start_side, start_line) = (d.start.map(|s| side_str(s.0)), d.start.map(|s| s.1));
        if d.id <= 0 {
            // owned by the account the repository is reached through (as `repo_account`),
            // so removing that account counts and keeps it
            conn.prepare_cached(
                "INSERT INTO drafts (repo_key, pr_id, kind, path, line, side, reply_to, text,
                                     source_commit, comment_type, start_side, start_line,
                                     account_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     COALESCE((SELECT account_id FROM repo_choices WHERE repo_key = ?1
                               ORDER BY selected DESC, account_id LIMIT 1), ?13))",
            )?
            .execute(params![
                repo,
                d.pr_id as i64,
                kind_str(d.kind),
                d.path,
                line,
                side,
                parent,
                d.text,
                d.source_commit,
                ctype,
                start_side,
                start_line,
                AccountId::default_bitbucket().as_str()
            ])?;
            return Ok(conn.last_insert_rowid());
        }
        let changed = conn
            .prepare_cached(
                "UPDATE drafts SET kind = ?1, path = ?2, line = ?3, side = ?4, reply_to = ?5,
                 text = ?6, source_commit = ?7, comment_type = ?11, start_side = ?12,
                 start_line = ?13
             WHERE id = ?8 AND repo_key = ?9 AND pr_id = ?10 AND posted_at IS NULL",
            )?
            .execute(params![
                kind_str(d.kind),
                d.path,
                line,
                side,
                parent,
                d.text,
                d.source_commit,
                d.id,
                repo,
                d.pr_id as i64,
                ctype,
                start_side,
                start_line
            ])?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(d.id)
    }

    pub fn list_drafts(&self, repo: &RepoRef, pr_id: u64) -> StoreResult<Vec<Draft>> {
        let conn = lock(&self.safe);
        let mut stmt = conn.prepare_cached(
            "SELECT id, kind, path, line, side, reply_to, text, source_commit, comment_type,
                    start_side, start_line
             FROM drafts WHERE repo_key = ?1 AND pr_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![key_of(repo), pr_id as i64], |r| {
            Ok((
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<u32>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ),
                (
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<u32>>(10)?,
                ),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                (id, kind, path, line, side, parent_id, text, source_commit),
                (ctype, start_side, start_line),
            ) = row?;
            // an unknown type (written by a newer build) reads as none; the draft is kept
            let comment_type = ctype.as_deref().and_then(CommentType::parse);
            // Rows with an unknown kind or side are skipped, not fatal for the whole list.
            let Some(kind) = parse_kind(&kind) else {
                continue;
            };
            let side = match side {
                Some(s) => match parse_side(&s) {
                    Some(s) => Some(s),
                    None => continue,
                },
                None => None,
            };
            // a half or unknown range start is skipped too: never posted as another range
            let start = match (start_side, start_line) {
                (None, None) => None,
                (Some(s), Some(l)) => match parse_side(&s) {
                    Some(s) => Some((s, l)),
                    None => continue,
                },
                _ => continue,
            };
            out.push(Draft {
                id,
                repo: repo.clone(),
                pr_id,
                kind,
                path,
                line,
                side,
                start,
                reply_to: parent_id.map(ThreadId),
                text,
                source_commit,
                comment_type,
            });
        }
        Ok(out)
    }

    /// Marks a draft as posted (a tombstone set right after the server accepted it, before the
    /// delete): a tombstoned draft is never posted again, even if the delete fails.
    pub fn mark_draft_posted(&self, id: i64) -> StoreResult<()> {
        let changed = lock(&self.safe)
            .prepare_cached("UPDATE drafts SET posted_at = datetime('now') WHERE id = ?1")?
            .execute(params![id])?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Ids of this PR's tombstoned (posted, not yet deleted) drafts.
    pub fn posted_draft_ids(&self, repo: &RepoRef, pr_id: u64) -> StoreResult<Vec<i64>> {
        let conn = lock(&self.safe);
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM drafts WHERE repo_key = ?1 AND pr_id = ?2 AND posted_at IS NOT NULL
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![key_of(repo), pr_id as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records that a draft is about to be sent (before the request). Refused for a missing
    /// or already posted draft.
    pub fn mark_draft_posting(&self, id: i64) -> StoreResult<()> {
        let changed = lock(&self.safe)
            .prepare_cached(
                "UPDATE drafts SET posting_started_at = datetime('now')
             WHERE id = ?1 AND posted_at IS NULL",
            )?
            .execute(params![id])?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Clears the "being sent" marker: the post definitely failed, or the user chose to post
    /// it again. A missing id is not an error.
    pub fn clear_draft_posting(&self, id: i64) -> StoreResult<()> {
        lock(&self.safe)
            .prepare_cached(
                "UPDATE drafts SET posting_started_at = NULL WHERE id = ?1 AND posted_at IS NULL",
            )?
            .execute(params![id])?;
        Ok(())
    }

    /// Ids of this PR's drafts that were sent without a known outcome (marker set, not
    /// posted).
    pub fn posting_draft_ids(&self, repo: &RepoRef, pr_id: u64) -> StoreResult<Vec<i64>> {
        let conn = lock(&self.safe);
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM drafts WHERE repo_key = ?1 AND pr_id = ?2
                 AND posting_started_at IS NOT NULL AND posted_at IS NULL
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![key_of(repo), pr_id as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Removes the draft; a missing id is not an error.
    pub fn delete_draft(&self, id: i64) -> StoreResult<()> {
        lock(&self.safe)
            .prepare_cached("DELETE FROM drafts WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }
}

/// Accounts (no secrets) and sync cursors, on the durable connection.
impl Store {
    /// Every account, ordered by id.
    pub fn accounts(&self) -> StoreResult<Vec<Account>> {
        let conn = lock(&self.safe);
        let mut stmt = conn.prepare_cached(
            "SELECT id, forge, web_base, api_base, graphql_base, git_host, user_id, login,
                 display_name, auth_kind, enabled
             FROM accounts ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                Account {
                    id: AccountId(r.get(0)?),
                    forge: Forge::BitbucketCloud,
                    web_base: r.get(2)?,
                    api_base: r.get(3)?,
                    graphql_base: r.get(4)?,
                    git_host: r.get(5)?,
                    user_id: r.get(6)?,
                    login: r.get(7)?,
                    display_name: r.get(8)?,
                    auth_kind: r.get(9)?,
                    enabled: r.get(10)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (_, forge, mut a) = row?;
            // an account of a forge this build does not know (a newer build) is left out
            let Some(f) = Forge::parse(&forge) else {
                continue;
            };
            a.forge = f;
            out.push(a);
        }
        Ok(out)
    }

    /// Adds `a`, or updates the account with its id.
    pub fn upsert_account(&self, a: &Account) -> StoreResult<()> {
        upsert_account_in(&lock(&self.safe), a)
    }

    /// Enables or disables an account (a disabled one is not synced; its selection stays).
    pub fn set_account_enabled(&self, id: &AccountId, enabled: bool) -> StoreResult<()> {
        let changed = lock(&self.safe)
            .prepare_cached("UPDATE accounts SET enabled = ?2 WHERE id = ?1")?
            .execute(params![id.as_str(), enabled])?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Removes account `id` and everything cached for it (its repository choices, PRs,
    /// diffstats, errors, cursors), in one transaction per connection. Its drafts are kept
    /// (and counted): they are the user's text. Other accounts' rows are never touched.
    pub fn remove_account(&self, id: &AccountId) -> StoreResult<RemovedCounts> {
        let acc = id.as_str();
        let mut out = RemovedCounts::default();
        {
            let conn = lock(&self.safe);
            let tx = write_tx(&conn)?;
            out.repo_choices = tx
                .prepare_cached("DELETE FROM repo_choices WHERE account_id = ?1")?
                .execute(params![acc])?;
            out.cursors = tx
                .prepare_cached("DELETE FROM sync_cursors WHERE account_id = ?1")?
                .execute(params![acc])?;
            out.drafts_kept = tx
                .prepare_cached(
                    "SELECT COUNT(*) FROM drafts WHERE account_id = ?1 AND posted_at IS NULL",
                )?
                .query_row(params![acc], |r| r.get::<_, i64>(0))?
                .max(0) as usize;
            tx.prepare_cached("DELETE FROM accounts WHERE id = ?1")?
                .execute(params![acc])?;
            tx.commit()?;
        }
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        out.prs = tx
            .prepare_cached("DELETE FROM prs WHERE account_id = ?1")?
            .execute(params![acc])?;
        out.diffstats = tx
            .prepare_cached("DELETE FROM diffstats WHERE account_id = ?1")?
            .execute(params![acc])?;
        out.repo_errors = tx
            .prepare_cached("DELETE FROM repo_errors WHERE account_id = ?1")?
            .execute(params![acc])?;
        tx.prepare_cached("DELETE FROM repos WHERE account_id = ?1")?
            .execute(params![acc])?;
        tx.commit()?;
        Ok(out)
    }

    /// What `remove_account(id)` would delete and keep, changing nothing (the removal
    /// confirmation shows it). The same row selections as `remove_account`, read on the read
    /// connection.
    pub fn removal_counts(&self, id: &AccountId) -> StoreResult<RemovedCounts> {
        let conn = lock(&self.read);
        let count = |sql: &str| -> StoreResult<usize> {
            let n: i64 = conn
                .prepare_cached(sql)?
                .query_row(params![id.as_str()], |r| r.get(0))?;
            Ok(n.max(0) as usize)
        };
        let owned = |table: &str| {
            count(&format!(
                "SELECT COUNT(*) FROM {table} WHERE account_id = ?1"
            ))
        };
        Ok(RemovedCounts {
            prs: owned("prs")?,
            diffstats: owned("diffstats")?,
            repo_choices: owned("repo_choices")?,
            repo_errors: owned("repo_errors")?,
            cursors: owned("sync_cursors")?,
            drafts_kept: count(
                "SELECT COUNT(*) FROM drafts WHERE account_id = ?1 AND posted_at IS NULL",
            )?,
        })
    }

    /// The stored sync position of `repo` under `account` for `kind` (e.g. "open").
    pub fn sync_cursor(
        &self,
        account: &AccountId,
        repo: &RepoRef,
        kind: &str,
    ) -> StoreResult<Option<SyncCursorRow>> {
        Ok(lock(&self.safe)
            .prepare_cached(
                "SELECT etag, last_updated FROM sync_cursors
                 WHERE account_id = ?1 AND repo_key = ?2 AND kind = ?3",
            )?
            .query_row(params![account.as_str(), key_of(repo), kind], |r| {
                Ok(SyncCursorRow {
                    etag: r.get(0)?,
                    last_updated: r.get(1)?,
                })
            })
            .optional()?)
    }

    pub fn set_sync_cursor(
        &self,
        account: &AccountId,
        repo: &RepoRef,
        kind: &str,
        cursor: &SyncCursorRow,
    ) -> StoreResult<()> {
        lock(&self.safe)
            .prepare_cached(
                "INSERT INTO sync_cursors (account_id, repo_key, kind, etag, last_updated,
                     checked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
                 ON CONFLICT(account_id, repo_key, kind) DO UPDATE SET etag = ?4,
                     last_updated = ?5, checked_at = datetime('now')",
            )?
            .execute(params![
                account.as_str(),
                key_of(repo),
                kind,
                cursor.etag,
                cursor.last_updated
            ])?;
        Ok(())
    }
}

/// The discovery cache and the user's repository selection (`repo_choices`, on the durable
/// connection: a choice is user data).
impl Store {
    /// Stores what one discovery listed, in one transaction. `listed_workspaces`: workspaces
    /// whose repository list was read in full; `all_workspaces`: the workspace list, when it
    /// was read. A stored repository that one of them proves vanished is marked gone when
    /// selected and removed otherwise; repositories of workspaces that could not be listed
    /// are left as they are. The selection itself never changes here.
    pub fn record_discovery(
        &self,
        listed: &[RepoInfo],
        listed_workspaces: &[String],
        all_workspaces: Option<&[String]>,
    ) -> StoreResult<()> {
        self.record_discovery_for(
            &AccountId::default_bitbucket(),
            listed,
            listed_workspaces,
            all_workspaces,
        )
    }

    /// `record_discovery` for the repositories `account` lists (its own rows only).
    pub fn record_discovery_for(
        &self,
        account: &AccountId,
        listed: &[RepoInfo],
        listed_workspaces: &[String],
        all_workspaces: Option<&[String]>,
    ) -> StoreResult<()> {
        let conn = lock(&self.safe);
        let tx = write_tx(&conn)?;
        let acc = account.as_str();
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO repo_choices (account_id, repo_key, host, namespace, slug, name,
                     is_private, discovered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))
                 ON CONFLICT(account_id, repo_key) DO UPDATE SET name = ?6, is_private = ?7,
                     discovered_at = datetime('now'), gone = 0",
            )?;
            for r in listed {
                stmt.execute(params![
                    acc,
                    key_of(&r.repo),
                    r.repo.host,
                    r.repo.namespace,
                    r.repo.slug,
                    r.name,
                    r.private
                ])?;
            }
        }
        let names: std::collections::HashSet<String> =
            listed.iter().map(|r| key_of(&r.repo)).collect();
        let stored: Vec<(String, String, bool)> = tx
            .prepare_cached(
                "SELECT repo_key, namespace, selected FROM repo_choices WHERE account_id = ?1",
            )?
            .query_map(params![acc], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (key, workspace, selected) in stored {
            let vanished = !names.contains(&key)
                && (listed_workspaces.contains(&workspace)
                    || all_workspaces.is_some_and(|all| !all.contains(&workspace)));
            if !vanished {
                continue;
            }
            let sql = if selected {
                "UPDATE repo_choices SET gone = 1 WHERE account_id = ?1 AND repo_key = ?2"
            } else {
                "DELETE FROM repo_choices WHERE account_id = ?1 AND repo_key = ?2"
            };
            tx.prepare_cached(sql)?.execute(params![acc, key])?;
        }
        Ok(tx.commit()?)
    }

    /// The discovery cache with the selection, ordered by workspace and repository.
    pub fn repo_choices(&self) -> StoreResult<Vec<RepoChoice>> {
        let conn = lock(&self.read);
        let mut stmt = conn.prepare_cached(
            "SELECT host, namespace, slug, name, is_private, selected, discovered_at, gone,
                 account_id
             FROM repo_choices ORDER BY namespace, slug, account_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RepoChoice {
                repo: repo_at(0)(r)?,
                name: r.get(3)?,
                private: r.get(4)?,
                selected: r.get(5)?,
                discovered_at: r.get(6)?,
                gone: r.get(7)?,
                account: AccountId(r.get(8)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The repositories a sync covers: selected and not gone, ordered by full name.
    pub fn selected_repos(&self) -> StoreResult<Vec<RepoRef>> {
        Ok(self
            .selected_repos_by_account()?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    /// `selected_repos` with the account each one syncs through (exactly one, decision D8).
    pub fn selected_repos_by_account(&self) -> StoreResult<Vec<(AccountId, RepoRef)>> {
        let conn = lock(&self.read);
        let mut stmt = conn.prepare_cached(
            "SELECT host, namespace, slug, account_id FROM repo_choices
             WHERE selected = 1 AND gone = 0
             ORDER BY namespace || '/' || slug, account_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((AccountId(r.get(3)?), repo_at(0)(r)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The account `repo` is reached through: the one that selected it, else one that
    /// listed it (the first by id); `None` when no account knows it.
    pub fn repo_account(&self, repo: &RepoRef) -> StoreResult<Option<AccountId>> {
        Ok(lock(&self.read)
            .prepare_cached(
                "SELECT account_id FROM repo_choices WHERE repo_key = ?1
                 ORDER BY selected DESC, account_id LIMIT 1",
            )?
            .query_row(params![key_of(repo)], |r| Ok(AccountId(r.get(0)?)))
            .optional()?)
    }

    /// True once any discovery stored its result (false on a new cache, so the first sign-in
    /// runs one).
    pub fn discovered_any(&self) -> StoreResult<bool> {
        Ok(lock(&self.read)
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM repo_choices WHERE discovered_at IS NOT NULL)",
            )?
            .query_row([], |r| r.get(0))?)
    }

    /// Makes `selected` the whole selection, in one transaction (a repository not in the
    /// cache yet is added). Gone repositories that are no longer selected are forgotten.
    pub fn set_selection(&self, selected: &[RepoRef]) -> StoreResult<()> {
        self.set_selection_for(&AccountId::default_bitbucket(), selected)
    }

    /// Makes `selected` the whole selection of `account`, in one transaction. A repository
    /// syncs through one account only (decision D8): selecting it here unselects it under
    /// every other account. Gone repositories that are no longer selected are forgotten.
    pub fn set_selection_for(&self, account: &AccountId, selected: &[RepoRef]) -> StoreResult<()> {
        let conn = lock(&self.safe);
        let tx = write_tx(&conn)?;
        let acc = account.as_str();
        tx.prepare_cached(
            "UPDATE repo_choices SET selected = 0 WHERE selected = 1 AND account_id = ?1",
        )?
        .execute(params![acc])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO repo_choices (account_id, repo_key, host, namespace, slug, selected)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(account_id, repo_key) DO UPDATE SET selected = 1",
            )?;
            let mut other = tx.prepare_cached(
                "UPDATE repo_choices SET selected = 0 WHERE repo_key = ?1 AND account_id <> ?2",
            )?;
            for r in selected {
                let key = key_of(r);
                stmt.execute(params![acc, key, r.host, r.namespace, r.slug])?;
                other.execute(params![key, acc])?;
            }
        }
        tx.execute_batch("DELETE FROM repo_choices WHERE selected = 0 AND gone = 1;")?;
        Ok(tx.commit()?)
    }

    /// Makes `selected` the whole selection of every account, in one transaction (the
    /// repository picker's Save). A repository listed twice is selected under its first
    /// account only (decision D8). Gone repositories no longer selected are forgotten.
    pub fn set_selection_all(&self, selected: &[(AccountId, RepoRef)]) -> StoreResult<()> {
        let conn = lock(&self.safe);
        let tx = write_tx(&conn)?;
        tx.execute_batch("UPDATE repo_choices SET selected = 0 WHERE selected = 1;")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO repo_choices (account_id, repo_key, host, namespace, slug, selected)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(account_id, repo_key) DO UPDATE SET selected = 1",
            )?;
            let mut seen = std::collections::HashSet::new();
            for (acc, r) in selected {
                let key = key_of(r);
                if seen.insert(key.clone()) {
                    stmt.execute(params![acc.as_str(), key, r.host, r.namespace, r.slug])?;
                }
            }
        }
        tx.execute_batch("DELETE FROM repo_choices WHERE selected = 0 AND gone = 1;")?;
        Ok(tx.commit()?)
    }

    /// Deletes the cached PRs, diffstats and sync errors of `repos` (drafts and viewed marks
    /// stay), in one transaction.
    pub fn purge_repos(&self, repos: &[RepoRef]) -> StoreResult<()> {
        let conn = lock(&self.fast);
        let tx = write_tx(&conn)?;
        for r in repos {
            let name = key_of(r);
            tx.prepare_cached("DELETE FROM prs WHERE repo_key = ?1")?
                .execute(params![name])?;
            tx.prepare_cached("DELETE FROM diffstats WHERE repo_key = ?1")?
                .execute(params![name])?;
            tx.prepare_cached("DELETE FROM repo_errors WHERE repo_key = ?1")?
                .execute(params![name])?;
        }
        Ok(tx.commit()?)
    }
}

/// Creates or upgrades the schema (additive steps only).
fn migrate(conn: &Connection) -> StoreResult<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    for (v, sql) in [
        (1, SCHEMA_V1),
        (2, SCHEMA_V2),
        (3, SCHEMA_V3),
        (4, SCHEMA_V4),
        (5, SCHEMA_V5),
        (6, SCHEMA_V6),
    ] {
        if version < v {
            let tx = write_tx(conn)?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", v)?;
            tx.commit()?;
        }
    }
    if version < 7 {
        let tx = write_tx(conn)?;
        migrate_v7(&tx)?;
        tx.pragma_update(None, "user_version", 7)?;
        tx.commit()?;
    }
    if version < 8 {
        let tx = write_tx(conn)?;
        tx.execute_batch(SCHEMA_V8)?;
        tx.pragma_update(None, "user_version", 8)?;
        tx.commit()?;
    }
    Ok(())
}

/// A v6 repo column value as a v7 key: `ws/slug` -> `bitbucket.org/ws/slug` (lower-case, as
/// `RepoRef::key`); the workspace-listing pseudo rows `*/*` and `ws/*` -> `<account>:*` and
/// `<account>:ws/*`.
fn v7_key(old: &str) -> String {
    let acc = DEFAULT_BITBUCKET_ACCOUNT;
    if old == "*/*" {
        format!("{acc}:*")
    } else if old.ends_with("/*") {
        format!("{acc}:{old}")
    } else {
        format!("{BITBUCKET_HOST}/{old}").to_lowercase()
    }
}

/// The v6 -> v7 upgrade, inside the caller's transaction (all of it, or none).
fn migrate_v7(tx: &Transaction) -> StoreResult<()> {
    // created iff the v6 cache belonged to a sign-in: an account in meta or any choice
    let owner = get_meta_in(tx, META_ACCOUNT_ID)?;
    let had_choices: bool =
        tx.query_row("SELECT EXISTS (SELECT 1 FROM repo_choices)", [], |r| {
            r.get(0)
        })?;
    tx.execute_batch(SCHEMA_V7)?;
    if owner.is_some() || had_choices {
        // display name from the remembered identity ("uuid\nname"), never a secret
        let name = get_meta_in(tx, "me")?
            .and_then(|m| m.split_once('\n').map(|(_, n)| n.to_string()))
            .unwrap_or_default();
        let d = Account::default_bitbucket(owner.unwrap_or_default(), name);
        upsert_account_in(tx, &d)?;
    }
    for (table, col) in [
        ("repos", "repo_key"),
        ("prs", "repo_key"),
        ("diffstats", "repo_key"),
        ("repo_errors", "repo_key"),
        ("viewed", "repo_key"),
        ("drafts", "repo_key"),
    ] {
        let olds: Vec<String> = tx
            .prepare(&format!("SELECT DISTINCT {col} FROM {table}"))?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for old in olds {
            let new = v7_key(&old);
            // two v6 names that differ only in case are one repository: the first one stays
            tx.execute(
                &format!("UPDATE OR IGNORE {table} SET {col} = ?1 WHERE {col} = ?2"),
                params![new, old],
            )?;
            tx.execute(
                &format!("DELETE FROM {table} WHERE {col} = ?1"),
                params![old],
            )?;
        }
    }
    // selected rows first, so a case duplicate keeps the selection
    let choices: Vec<ChoiceRowV6> = tx
        .prepare(
            "SELECT full_name, workspace, slug, name, is_private, selected, discovered_at, gone
             FROM repo_choices ORDER BY selected DESC, full_name",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (full_name, ns, slug, name, private, selected, discovered_at, gone) in choices {
        tx.execute(
            "INSERT OR IGNORE INTO repo_choices_v7 (account_id, repo_key, host, namespace, slug,
                 name, is_private, selected, discovered_at, gone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                DEFAULT_BITBUCKET_ACCOUNT,
                v7_key(&full_name),
                BITBUCKET_HOST,
                ns,
                slug,
                name,
                private,
                selected,
                discovered_at,
                gone
            ],
        )?;
    }
    tx.execute_batch(
        "DROP TABLE repo_choices;
         ALTER TABLE repo_choices_v7 RENAME TO repo_choices;",
    )?;
    Ok(())
}

/// A v6 `repo_choices` row: full_name, workspace, slug, name, is_private, selected,
/// discovered_at, gone.
type ChoiceRowV6 = (
    String,
    String,
    String,
    Option<String>,
    Option<bool>,
    bool,
    Option<String>,
    bool,
);

/// (id, state, updated_on, json) as stored for a PR.
type PrRow = (i64, String, String, String);

fn pr_row(pr: &PullRequest) -> StoreResult<PrRow> {
    let state = serde_json::to_value(&pr.state)?
        .as_str()
        .map(str::to_owned)
        .ok_or(StoreError::Corrupt)?;
    Ok((
        pr.id as i64,
        state,
        pr.updated_on.to_rfc3339(),
        serde_json::to_string(pr)?,
    ))
}

/// Replaces `repo`'s PRs with `prs` owned by `acc`, unless the stored rows already equal the
/// new ones. Rows of the repository owned by another account (it moved, D8) are dropped.
fn replace_prs_in(
    conn: &Connection,
    acc: &str,
    repo: &RepoRef,
    prs: &[PullRequest],
) -> StoreResult<()> {
    let name = key_of(repo);
    conn.prepare_cached("DELETE FROM prs WHERE repo_key = ?1 AND account_id <> ?2")?
        .execute(params![name, acc])?;
    let mut rows = prs.iter().map(pr_row).collect::<StoreResult<Vec<_>>>()?;
    rows.sort_by_key(|r| r.0);
    let stored: Vec<PrRow> = conn
        .prepare_cached(
            "SELECT id, state, updated_on, json FROM prs WHERE repo_key = ?1 ORDER BY id",
        )?
        .query_map(params![name], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    if stored == rows {
        return Ok(());
    }
    conn.prepare_cached("DELETE FROM prs WHERE repo_key = ?1")?
        .execute(params![name])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO prs (repo_key, id, state, updated_on, json, account_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (id, state, updated_on, json) in rows {
        insert.execute(params![name, id, state, updated_on, json, acc])?;
    }
    Ok(())
}

fn put_diffstat_in(
    conn: &Connection,
    acc: &str,
    repo: &RepoRef,
    id: u64,
    source_commit: &str,
    stat: &DiffStat,
) -> StoreResult<()> {
    conn.prepare_cached(
        "INSERT INTO diffstats (repo_key, id, source_commit, json, account_id)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_key, id) DO UPDATE SET source_commit = ?3, json = ?4, account_id = ?5
         WHERE source_commit IS NOT ?3 OR json IS NOT ?4 OR account_id IS NOT ?5",
    )?
    .execute(params![
        key_of(repo),
        id as i64,
        source_commit,
        serde_json::to_string(stat)?,
        acc
    ])?;
    Ok(())
}

fn set_repo_error_in(
    conn: &Connection,
    acc: &str,
    repo: &RepoRef,
    message: Option<&str>,
) -> StoreResult<()> {
    let key = key_for(acc, repo);
    match message {
        Some(m) => conn
            .prepare_cached(
                "INSERT INTO repo_errors (repo_key, namespace, slug, message, host, account_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(repo_key) DO UPDATE SET message = ?4, account_id = ?6
                 WHERE message IS NOT ?4 OR account_id IS NOT ?6",
            )?
            .execute(params![key, repo.namespace, repo.slug, m, repo.host, acc])?,
        None => conn
            .prepare_cached("DELETE FROM repo_errors WHERE repo_key = ?1")?
            .execute(params![key])?,
    };
    Ok(())
}

fn repo_errors_in(conn: &Connection) -> StoreResult<Vec<(RepoRef, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT host, namespace, slug, message FROM repo_errors
         ORDER BY namespace || '/' || slug",
    )?;
    let rows = stmt.query_map([], |r| Ok((repo_at(0)(r)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The store key of `repo` (`RepoRef::key`). A workspace-listing pseudo repository (slug
/// `*`) is the default account's `<account>:*` / `<account>:ws/*` row.
fn key_of(repo: &RepoRef) -> String {
    key_for(DEFAULT_BITBUCKET_ACCOUNT, repo)
}

/// `key_of` with the listing pseudo rows (slug `*`) of account `acc`.
fn key_for(acc: &str, repo: &RepoRef) -> String {
    if repo.slug == "*" {
        if repo.namespace == "*" {
            format!("{acc}:*")
        } else {
            format!("{acc}:{}/*", repo.namespace)
        }
    } else {
        repo.key()
    }
}

/// Reads a `RepoRef` from columns `host, namespace, slug` starting at `i`.
fn repo_at(i: usize) -> impl Fn(&rusqlite::Row) -> rusqlite::Result<RepoRef> {
    move |r| {
        Ok(RepoRef {
            host: r.get(i)?,
            namespace: r.get(i + 1)?,
            slug: r.get(i + 2)?,
        })
    }
}

fn upsert_account_in(conn: &Connection, a: &Account) -> StoreResult<()> {
    conn.prepare_cached(
        "INSERT INTO accounts (id, forge, web_base, api_base, graphql_base, git_host, user_id,
             login, display_name, auth_kind, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET forge = ?2, web_base = ?3, api_base = ?4,
             graphql_base = ?5, git_host = ?6, user_id = ?7, login = ?8, display_name = ?9,
             auth_kind = ?10, enabled = ?11",
    )?
    .execute(params![
        a.id.as_str(),
        a.forge.as_str(),
        a.web_base,
        a.api_base,
        a.graphql_base,
        a.git_host,
        a.user_id,
        a.login,
        a.display_name,
        a.auth_kind,
        a.enabled
    ])?;
    Ok(())
}

fn set_meta_in(conn: &Connection, key: &str, value: &str) -> StoreResult<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
    )?
    .execute(params![key, value])?;
    Ok(())
}

fn delete_meta_in(conn: &Connection, key: &str) -> StoreResult<()> {
    conn.prepare_cached("DELETE FROM meta WHERE key = ?1")?
        .execute(params![key])?;
    Ok(())
}

fn get_meta_in(conn: &Connection, key: &str) -> StoreResult<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row(params![key], |r| r.get(0))
        .optional()?)
}

fn set_viewed_in(
    conn: &Connection,
    repo: &RepoRef,
    pr_id: u64,
    path: &str,
    oid: &str,
    viewed: bool,
) -> StoreResult<()> {
    if viewed {
        conn.prepare_cached(
            "INSERT INTO viewed (repo_key, pr_id, path, oid) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repo_key, pr_id, path) DO UPDATE SET oid = ?4",
        )?
        .execute(params![key_of(repo), pr_id as i64, path, oid])?;
    } else {
        conn.prepare_cached("DELETE FROM viewed WHERE repo_key = ?1 AND pr_id = ?2 AND path = ?3")?
            .execute(params![key_of(repo), pr_id as i64, path])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
