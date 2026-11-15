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
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use prcore::model::{PrState, ReviewState, Reviewer, UserRef};

    impl Store {
        fn safe(&self) -> MutexGuard<'_, Connection> {
            lock(&self.safe)
        }

        /// Rows changed so far through the connection of the rebuildable tables.
        fn fast_changes(&self) -> u64 {
            lock(&self.fast).total_changes()
        }
    }

    fn repo(ws: &str, slug: &str) -> RepoRef {
        RepoRef {
            host: prcore::model::BITBUCKET_HOST.into(),
            namespace: ws.into(),
            slug: slug.into(),
        }
    }

    fn user(n: &str) -> UserRef {
        UserRef {
            id: format!("{{{n}}}"),
            display_name: n.into(),
            login: None,
        }
    }

    fn pr(r: &RepoRef, id: u64, state: PrState) -> PullRequest {
        PullRequest {
            repo: r.clone(),
            id,
            title: format!("PR {id}"),
            state,
            draft: true,
            author: user("alice"),
            source_branch: "feat".into(),
            dest_branch: "main".into(),
            source_commit: "abc123".into(),
            dest_commit: "def456".into(),
            source_repo: repo("fork", "api"),
            comment_count: 3,
            task_count: 2,
            reviewers: vec![
                Reviewer {
                    user: user("bob"),
                    state: ReviewState::Approved,
                },
                Reviewer {
                    user: user("carol"),
                    state: ReviewState::ChangesRequested,
                },
            ],
            created_on: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            updated_on: Utc.with_ymd_and_hms(2026, 2, 3, 4, 5, 6).unwrap(),
            forge: Default::default(),
            account: Default::default(),
            web_url: String::new(),
            node_id: None,
            review_requested_from_me: false,
            locked: false,
        }
    }

    #[test]
    fn repos_round_trip() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_repos(&[repo("b", "z"), repo("a", "y")]).unwrap();
        s.upsert_repos(&[repo("a", "y")]).unwrap();
        assert_eq!(
            s.list_repos().unwrap(),
            vec![repo("a", "y"), repo("b", "z")]
        );
    }

    #[test]
    fn replace_prs_removes_prs_no_longer_returned() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let other = repo("acme", "web");
        s.replace_prs(&r, &[pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Merged)])
            .unwrap();
        s.replace_prs(&other, &[pr(&other, 9, PrState::Open)])
            .unwrap();
        s.replace_prs(&r, &[pr(&r, 2, PrState::Merged), pr(&r, 3, PrState::Open)])
            .unwrap();
        let ids: Vec<(String, u64)> = s
            .list_prs()
            .unwrap()
            .iter()
            .map(|p| (p.repo.full_name(), p.id))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("acme/api".to_string(), 2),
                ("acme/api".to_string(), 3),
                ("acme/web".to_string(), 9)
            ]
        );
    }

    #[test]
    fn set_pr_reviewers_updates_only_that_pr() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        s.replace_prs(&r, &[pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)])
            .unwrap();
        let mine = vec![Reviewer {
            user: user("me"),
            state: ReviewState::Approved,
        }];
        assert!(s.set_pr_reviewers(&r, 2, &mine).unwrap());
        assert!(!s.set_pr_reviewers(&r, 9, &mine).unwrap(), "not cached");
        let prs = s.list_prs().unwrap();
        assert_eq!(
            prs[0],
            pr(&r, 1, PrState::Open),
            "the other PR is untouched"
        );
        let want = PullRequest {
            reviewers: mine,
            ..pr(&r, 2, PrState::Open)
        };
        assert_eq!(prs[1], want);
    }

    #[test]
    fn replace_prs_is_atomic_on_failure() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let old = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)];
        s.replace_prs(&r, &old).unwrap();
        // Failure is induced by passing two PRs with the same id: the second insert violates the
        // (repo, id) primary key after the old rows were deleted and the first row inserted, so
        // the transaction must roll back and leave the old rows untouched.
        let bad = vec![pr(&r, 5, PrState::Open), pr(&r, 5, PrState::Merged)];
        assert!(s.replace_prs(&r, &bad).is_err());
        assert_eq!(s.list_prs().unwrap(), old);
    }

    #[test]
    fn prs_round_trip_all_fields() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let prs = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Superseded)];
        s.replace_prs(&r, &prs).unwrap();
        assert_eq!(s.list_prs().unwrap(), prs);
    }

    #[test]
    fn diffstat_round_trip_with_commit() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let st = DiffStat {
            files: 3,
            lines_added: 10,
            lines_removed: 4,
            max_file_lines: 7,
        };
        s.put_diffstat(&r, 1, "c1", &st).unwrap();
        assert_eq!(
            s.get_diffstat(&r, 1).unwrap(),
            Some(("c1".to_string(), st.clone()))
        );
        let st2 = DiffStat { files: 1, ..st };
        s.put_diffstat(&r, 1, "c2", &st2).unwrap();
        assert_eq!(
            s.get_diffstat(&r, 1).unwrap(),
            Some(("c2".to_string(), st2))
        );
    }

    #[test]
    fn diffstat_missing_is_none() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.get_diffstat(&repo("a", "b"), 1).unwrap(), None);
    }

    #[test]
    fn repo_error_set_and_cleared() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        s.set_repo_error(&r, Some("boom")).unwrap();
        assert_eq!(
            s.repo_errors().unwrap(),
            vec![(r.clone(), "boom".to_string())]
        );
        s.set_repo_error(&r, Some("bang")).unwrap();
        assert_eq!(
            s.repo_errors().unwrap(),
            vec![(r.clone(), "bang".to_string())]
        );
        s.set_repo_error(&r, None).unwrap();
        assert!(s.repo_errors().unwrap().is_empty());
    }

    #[test]
    fn meta_round_trip() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.get_meta("k").unwrap(), None);
        s.set_meta("k", "v1").unwrap();
        s.set_meta("k", "v2").unwrap();
        assert_eq!(s.get_meta("k").unwrap(), Some("v2".to_string()));
        s.delete_meta("k").unwrap();
        assert_eq!(s.get_meta("k").unwrap(), None);
        s.delete_meta("k").unwrap();
    }

    #[test]
    fn reopening_file_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_repos(std::slice::from_ref(&r)).unwrap();
            s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
            s.set_meta("k", "v").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
        assert_eq!(s.list_prs().unwrap(), vec![pr(&r, 1, PrState::Open)]);
        assert_eq!(s.get_meta("k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        drop(Store::open(&path).unwrap());
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 999).unwrap();
        drop(c);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema { found: 999, .. })
        ));
    }

    fn draft(r: &RepoRef, pr_id: u64, text: &str) -> Draft {
        Draft {
            id: 0,
            repo: r.clone(),
            pr_id,
            kind: DraftKind::Inline,
            path: Some("src/a.rs".into()),
            line: Some(12),
            side: Some(Side::Old),
            start: None,
            reply_to: None,
            text: text.into(),
            source_commit: "abc123".into(),
            comment_type: None,
        }
    }

    #[test]
    fn viewed_roundtrip_and_toggle_off() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        assert!(s.viewed_map(&r, 1).unwrap().is_empty());
        s.set_viewed(&r, 1, "a.rs", "oid1", true).unwrap();
        s.set_viewed(&r, 1, "b.rs", "oid2", true).unwrap();
        s.set_viewed(&r, 1, "a.rs", "oid3", true).unwrap();
        let m = s.viewed_map(&r, 1).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["a.rs"], "oid3");
        assert_eq!(m["b.rs"], "oid2");
        s.set_viewed(&r, 1, "a.rs", "oid3", false).unwrap();
        s.set_viewed(&r, 1, "a.rs", "oid3", false).unwrap();
        let m = s.viewed_map(&r, 1).unwrap();
        assert_eq!(m.len(), 1);
        assert!(!m.contains_key("a.rs"));
    }

    #[test]
    fn viewed_many_sets_and_clears_in_one_go() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        s.set_viewed(&r, 1, "c.rs", "oc", true).unwrap();
        s.set_viewed_many(
            &r,
            1,
            &[
                ("a.rs", "oa", true),
                ("b.rs", "ob", true),
                ("c.rs", "oc", false),
            ],
        )
        .unwrap();
        let m = s.viewed_map(&r, 1).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["a.rs"], "oa");
        assert_eq!(m["b.rs"], "ob");
        s.set_viewed_many(&r, 1, &[]).unwrap();
        assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 2);
    }

    #[test]
    fn viewed_scoped_per_pr() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let other = repo("acme", "web");
        s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
        assert!(s.viewed_map(&r, 2).unwrap().is_empty());
        assert!(s.viewed_map(&other, 1).unwrap().is_empty());
        assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 1);
    }

    #[test]
    fn draft_crud_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let (id1, id2);
        {
            let s = Store::open(&path).unwrap();
            id1 = s.save_draft(&draft(&r, 1, "first")).unwrap();
            let mut reply = draft(&r, 1, "second");
            reply.kind = DraftKind::Reply;
            reply.reply_to = Some(ThreadId::from(77));
            reply.path = None;
            reply.line = None;
            reply.side = None;
            id2 = s.save_draft(&reply).unwrap();
            assert_ne!(id1, id2);
            let mut edited = draft(&r, 1, "first edited");
            edited.id = id1;
            edited.side = Some(Side::New);
            assert_eq!(s.save_draft(&edited).unwrap(), id1);
        }
        let s = Store::open(&path).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].id, id1);
        assert_eq!(ds[0].text, "first edited");
        assert_eq!(ds[0].side, Some(Side::New));
        assert_eq!(ds[0].line, Some(12));
        assert_eq!(ds[1].id, id2);
        assert_eq!(ds[1].kind, DraftKind::Reply);
        assert_eq!(ds[1].reply_to, Some(ThreadId::from(77)));
        assert_eq!(ds[1].path, None);
        s.delete_draft(id1).unwrap();
        s.delete_draft(id1).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].id, id2);
    }

    #[test]
    fn drafts_scoped_per_pr() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let other = repo("acme", "web");
        s.save_draft(&draft(&r, 1, "x")).unwrap();
        s.save_draft(&draft(&r, 2, "y")).unwrap();
        s.save_draft(&draft(&other, 1, "z")).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].text, "x");
        assert_eq!(s.list_drafts(&other, 1).unwrap()[0].text, "z");
        assert!(s.list_drafts(&other, 2).unwrap().is_empty());
    }

    #[test]
    fn settings_defaults_then_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let default = Path::new("C:/data/mirrors");
        let st = s.settings(default).unwrap();
        assert_eq!(st.clone_root, None);
        assert_eq!(st.download_dir, default);
        let new = Settings {
            clone_root: Some(PathBuf::from("D:/code")),
            download_dir: PathBuf::from("E:/mirrors"),
        };
        s.save_settings(&new).unwrap();
        assert_eq!(s.settings(default).unwrap(), new);
        let cleared = Settings {
            clone_root: None,
            ..new
        };
        s.save_settings(&cleared).unwrap();
        assert_eq!(s.settings(default).unwrap(), cleared);
    }

    #[test]
    fn migration_from_v1_keeps_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let p = pr(&r, 1, PrState::Open);
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
            c.execute(
                "INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/api', 'acme', 'api')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO prs (repo, id, state, updated_on, json) VALUES (?1, 1, 'OPEN', ?2, ?3)",
                params![
                    "acme/api",
                    p.updated_on.to_rfc3339(),
                    serde_json::to_string(&p).unwrap()
                ],
            )
            .unwrap();
            c.execute("INSERT INTO meta (key, value) VALUES ('k', 'v')", [])
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
        assert_eq!(s.list_prs().unwrap(), vec![p]);
        assert_eq!(s.get_meta("k").unwrap(), Some("v".to_string()));
        let v: i64 = s
            .safe()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
        assert!(s.save_draft(&draft(&r, 1, "t")).unwrap() > 0);
    }

    #[test]
    fn posted_tombstone_survives_reopen_and_blocks_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let (a, b);
        {
            let s = Store::open(&path).unwrap();
            a = s.save_draft(&draft(&r, 1, "a")).unwrap();
            b = s.save_draft(&draft(&r, 1, "b")).unwrap();
            s.mark_draft_posted(a).unwrap();
            assert!(matches!(
                s.mark_draft_posted(999),
                Err(StoreError::NotFound)
            ));
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![a]);
        assert!(s.posted_draft_ids(&r, 2).unwrap().is_empty());
        // a posted draft cannot be edited back into a postable one
        let mut edited = draft(&r, 1, "changed");
        edited.id = a;
        assert!(matches!(s.save_draft(&edited), Err(StoreError::NotFound)));
        edited.id = b;
        assert_eq!(s.save_draft(&edited).unwrap(), b);
        s.delete_draft(a).unwrap();
        assert!(s.posted_draft_ids(&r, 1).unwrap().is_empty());
    }

    #[test]
    fn migration_from_v2_keeps_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(SCHEMA_V2).unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
            c.execute(
                "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
                 VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 1);
        assert!(s.posted_draft_ids(&r, 1).unwrap().is_empty());
        s.mark_draft_posted(ds[0].id).unwrap();
        assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![ds[0].id]);
    }

    #[test]
    fn migration_from_v3_keeps_drafts_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(SCHEMA_V2).unwrap();
            c.execute_batch(SCHEMA_V3).unwrap();
            c.pragma_update(None, "user_version", 3).unwrap();
            c.execute(
                "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
                 VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posted_at)
                 VALUES ('acme/api', 1, 'pr', 'posted', 'h', datetime('now'))",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 2);
        assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![ds[1].id]);
        assert!(s.posting_draft_ids(&r, 1).unwrap().is_empty());
        let v: i64 = s
            .safe()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    /// A v4 cache file holding PRs of `with_prs` (and a repo row without PRs).
    fn v4_cache(path: &Path, with_prs: &[RepoRef]) {
        let c = rusqlite::Connection::open(path).unwrap();
        for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 4).unwrap();
        c.execute(
            "INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/idle', 'acme', 'idle')",
            [],
        )
        .unwrap();
        for (i, r) in with_prs.iter().enumerate() {
            let p = pr(r, i as u64 + 1, PrState::Open);
            for id in [p.id, p.id + 100] {
                c.execute(
                    "INSERT INTO prs (repo, id, state, updated_on, json) VALUES (?1, ?2, 'OPEN', ?3, ?4)",
                    params![
                        r.full_name(),
                        id as i64,
                        p.updated_on.to_rfc3339(),
                        serde_json::to_string(&p).unwrap()
                    ],
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn upgrade_preselects_repos_with_cached_prs_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let (api, web) = (repo("acme", "api"), repo("beta", "web"));
        v4_cache(&path, &[api.clone(), web.clone()]);
        let s = Store::open(&path).unwrap();
        // nothing disappears: both repos with PRs are selected, the idle one is not
        assert_eq!(s.selected_repos().unwrap(), vec![api.clone(), web.clone()]);
        let snap = s.cache_snapshot().unwrap();
        assert_eq!(snap.prs.len(), 4);
        assert_eq!((snap.repo_count, snap.preselected), (2, Some(2)));
        assert!(!s.discovered_any().unwrap(), "no discovery has run yet");
        let choices = s.repo_choices().unwrap();
        assert!(choices
            .iter()
            .all(|c| c.selected && c.discovered_at.is_none()));
        // once: the user unselects one and dismisses the notice; a reopen changes nothing
        s.set_selection(std::slice::from_ref(&api)).unwrap();
        s.delete_meta(META_REPOS_PRESELECTED).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.selected_repos().unwrap(), vec![api]);
        assert_eq!(s.cache_snapshot().unwrap().preselected, None);
    }

    #[test]
    fn a_new_cache_selects_nothing_and_has_no_notice() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.selected_repos().unwrap().is_empty());
        let snap = s.cache_snapshot().unwrap();
        assert_eq!((snap.repo_count, snap.preselected), (0, None));
        // an upgrade of a cache without PRs selects nothing either
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        v4_cache(&path, &[]);
        let s = Store::open(&path).unwrap();
        assert!(s.repo_choices().unwrap().is_empty());
        assert_eq!(s.get_meta(META_REPOS_PRESELECTED).unwrap(), None);
    }

    fn info(ws: &str, slug: &str, private: bool) -> RepoInfo {
        RepoInfo {
            repo: repo(ws, slug),
            name: Some(slug.to_uppercase()),
            private: Some(private),
        }
    }

    #[test]
    fn discovery_is_cached_and_never_selects() {
        let s = Store::open_in_memory().unwrap();
        let ws = vec!["acme".to_string()];
        s.record_discovery(
            &[info("acme", "web", false), info("acme", "api", true)],
            &ws,
            Some(&ws),
        )
        .unwrap();
        assert!(s.discovered_any().unwrap());
        let c = s.repo_choices().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].repo, repo("acme", "api"));
        assert_eq!(
            (c[0].name.as_deref(), c[0].private, c[0].selected, c[0].gone),
            (Some("API"), Some(true), false, false)
        );
        assert!(c[0].discovered_at.is_some());
        assert!(s.selected_repos().unwrap().is_empty());
        let snap = s.cache_snapshot().unwrap();
        assert_eq!((snap.repo_count, snap.known_repos), (0, 2));
        // a selection survives the next discovery
        s.set_selection(&[repo("acme", "api")]).unwrap();
        s.record_discovery(
            &[info("acme", "web", false), info("acme", "api", true)],
            &ws,
            Some(&ws),
        )
        .unwrap();
        assert_eq!(s.selected_repos().unwrap(), vec![repo("acme", "api")]);
    }

    #[test]
    fn vanished_repos_are_marked_gone_when_selected_and_dropped_otherwise() {
        let s = Store::open_in_memory().unwrap();
        let ws = vec!["acme".to_string(), "beta".to_string()];
        s.record_discovery(
            &[
                info("acme", "api", false),
                info("acme", "web", false),
                info("acme", "old", false),
                info("beta", "x", false),
                info("gamma", "y", false),
            ],
            &ws,
            None,
        )
        .unwrap();
        s.set_selection(&[repo("acme", "api"), repo("beta", "x"), repo("gamma", "y")])
            .unwrap();
        // acme no longer lists api (selected) nor old (not selected); beta could not be
        // listed; gamma is no longer one of my workspaces
        let all = vec!["acme".to_string(), "beta".to_string()];
        s.record_discovery(
            &[info("acme", "web", false)],
            &["acme".to_string()],
            Some(&all),
        )
        .unwrap();
        let c = s.repo_choices().unwrap();
        let names: Vec<(String, bool, bool)> = c
            .iter()
            .map(|c| (c.repo.full_name(), c.selected, c.gone))
            .collect();
        assert_eq!(
            names,
            vec![
                ("acme/api".to_string(), true, true),
                ("acme/web".to_string(), false, false),
                ("beta/x".to_string(), true, false),
                ("gamma/y".to_string(), true, true),
            ]
        );
        // a gone repo is not synced, and forgotten once unselected
        assert_eq!(s.selected_repos().unwrap(), vec![repo("beta", "x")]);
        s.set_selection(&[repo("beta", "x"), repo("gamma", "y")])
            .unwrap();
        assert!(!s
            .repo_choices()
            .unwrap()
            .iter()
            .any(|c| c.repo == repo("acme", "api")));
        // listed again: accessible again, still selected
        s.record_discovery(&[info("gamma", "y", false)], &["gamma".to_string()], None)
            .unwrap();
        assert_eq!(
            s.selected_repos().unwrap(),
            vec![repo("beta", "x"), repo("gamma", "y")]
        );
    }

    #[test]
    fn the_inbox_snapshot_shows_only_selected_repos() {
        let s = Store::open_in_memory().unwrap();
        let (a, b) = (repo("acme", "api"), repo("acme", "web"));
        s.apply_repo_sync(&a, Some(&[pr(&a, 1, PrState::Open)]), &[], Some("http 500"))
            .unwrap();
        s.apply_repo_sync(
            &b,
            Some(&[pr(&b, 2, PrState::Open)]),
            &[],
            Some("rate limited"),
        )
        .unwrap();
        s.set_repo_error(&repo("acme", "*"), Some("network error"))
            .unwrap();
        s.set_selection(std::slice::from_ref(&a)).unwrap();
        let snap = s.cache_snapshot().unwrap();
        let shown: Vec<&RepoRef> = snap.prs.iter().map(|(p, _)| &p.repo).collect();
        assert_eq!(shown, vec![&a]);
        assert_eq!(
            snap.errors,
            vec![
                (repo("acme", "*"), "network error".to_string()),
                (a.clone(), "http 500".to_string())
            ]
        );
        // unselected: hidden, still cached
        assert_eq!(s.list_prs().unwrap().len(), 2);
        // purged: gone from the cache, drafts and viewed marks kept
        s.save_draft(&draft(&b, 2, "keep")).unwrap();
        s.set_viewed(&b, 2, "a.rs", "o", true).unwrap();
        s.put_diffstat(&b, 2, "c", &st(1)).unwrap();
        s.purge_repos(std::slice::from_ref(&b)).unwrap();
        assert_eq!(s.list_prs().unwrap().len(), 1);
        assert_eq!(s.get_diffstat(&b, 2).unwrap(), None);
        assert_eq!(s.repo_errors().unwrap().len(), 2);
        assert_eq!(s.list_drafts(&b, 2).unwrap().len(), 1);
        assert_eq!(s.viewed_map(&b, 2).unwrap().len(), 1);
    }

    #[test]
    fn choices_follow_the_account() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        s.set_selection(std::slice::from_ref(&r)).unwrap();
        s.set_meta(META_REPOS_PRESELECTED, "1").unwrap();
        // no account recorded yet (an upgraded cache): the choices are kept
        assert!(s
            .adopt_user(&AccountId::default_bitbucket(), "{a}", "")
            .unwrap());
        assert_eq!(s.selected_repos().unwrap(), vec![r.clone()]);
        assert!(!s
            .adopt_user(&AccountId::default_bitbucket(), "{a}", "")
            .unwrap());
        assert_eq!(s.selected_repos().unwrap(), vec![r]);
        // another account: none of the first one's choices
        assert!(s
            .adopt_user(&AccountId::default_bitbucket(), "{b}", "")
            .unwrap());
        assert!(s.repo_choices().unwrap().is_empty());
        assert_eq!(s.get_meta(META_REPOS_PRESELECTED).unwrap(), None);
    }

    #[test]
    fn migration_from_v4_keeps_drafts_untyped_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 4).unwrap();
            c.execute(
                "INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posting_started_at)
                 VALUES ('acme/api', 1, 'pr', 'old one', 'h', datetime('now'))",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].text, "old one");
        assert_eq!(ds[0].comment_type, None, "old drafts have no type");
        assert_eq!(ds[0].posted_text(), "old one");
        assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![ds[0].id]);
        let v: i64 = s
            .safe()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn draft_type_round_trips_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let id;
        {
            let s = Store::open(&path).unwrap();
            let mut d = draft(&r, 1, "rename");
            d.comment_type = Some(CommentType::Nit);
            id = s.save_draft(&d).unwrap();
            let mut q = draft(&r, 1, "why?");
            q.comment_type = Some(CommentType::Question);
            s.save_draft(&q).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds[0].comment_type, Some(CommentType::Nit));
        assert_eq!(ds[1].comment_type, Some(CommentType::Question));
        // the text is stored without the prefix; it is added at post time only
        assert_eq!(ds[0].text, "rename");
        assert_eq!(ds[0].posted_text(), "**nit:** rename");
        let mut edited = ds[0].clone();
        edited.comment_type = None;
        assert_eq!(s.save_draft(&edited).unwrap(), id);
        assert_eq!(s.list_drafts(&r, 1).unwrap()[0].comment_type, None);
        // an unknown stored type (a newer build) reads as no type, the draft is kept
        s.safe()
            .execute(
                "UPDATE drafts SET comment_type = 'blocker' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].comment_type, None);
    }

    #[test]
    fn posting_marker_survives_reopen_and_is_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let (a, b);
        {
            let s = Store::open(&path).unwrap();
            a = s.save_draft(&draft(&r, 1, "a")).unwrap();
            b = s.save_draft(&draft(&r, 1, "b")).unwrap();
            s.mark_draft_posting(a).unwrap();
            s.mark_draft_posting(b).unwrap();
            // a posted (tombstoned) draft gets no new marker
            s.mark_draft_posted(b).unwrap();
            assert!(matches!(s.mark_draft_posting(b), Err(StoreError::NotFound)));
            assert!(matches!(
                s.mark_draft_posting(999),
                Err(StoreError::NotFound)
            ));
        }
        let s = Store::open(&path).unwrap();
        // posted ones are not "posting": only a, which has no outcome yet
        assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![a]);
        assert!(s.posting_draft_ids(&r, 2).unwrap().is_empty());
        s.clear_draft_posting(a).unwrap();
        assert!(s.posting_draft_ids(&r, 1).unwrap().is_empty());
        // editing keeps the marker: an edit is not a re-arm
        s.mark_draft_posting(a).unwrap();
        let mut edited = draft(&r, 1, "edited");
        edited.id = a;
        s.save_draft(&edited).unwrap();
        assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![a]);
    }

    #[test]
    fn save_after_delete_returns_not_found() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let mut d = draft(&r, 1, "x");
        d.id = s.save_draft(&d).unwrap();
        s.delete_draft(d.id).unwrap();
        assert!(matches!(s.save_draft(&d), Err(StoreError::NotFound)));
        assert!(s.list_drafts(&r, 1).unwrap().is_empty());
    }

    #[test]
    fn save_cannot_move_draft_to_other_pr() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let mut d = draft(&r, 1, "x");
        d.id = s.save_draft(&d).unwrap();
        let mut moved = d.clone();
        moved.pr_id = 2;
        assert!(matches!(s.save_draft(&moved), Err(StoreError::NotFound)));
        moved.pr_id = 1;
        moved.repo = repo("acme", "web");
        assert!(matches!(s.save_draft(&moved), Err(StoreError::NotFound)));
        assert_eq!(s.list_drafts(&r, 1).unwrap(), vec![d]);
        assert!(s.list_drafts(&r, 2).unwrap().is_empty());
    }

    #[test]
    fn list_skips_corrupt_row() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let good = s.save_draft(&draft(&r, 1, "good")).unwrap();
        for (kind, side) in [("bogus", None), ("inline", Some("sideways"))] {
            s.safe()
                .execute(
                    "INSERT INTO drafts (repo_key, pr_id, kind, side, text, source_commit)
                     VALUES ('bitbucket.org/acme/api', 1, ?1, ?2, 't', 'c')",
                    params![kind, side],
                )
                .unwrap();
        }
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].id, good);
    }

    #[test]
    fn no_secret_columns_exist() {
        let s = Store::open_in_memory().unwrap();
        let tables: Vec<String> = s
            .safe()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!tables.is_empty());
        for t in tables {
            let cols: Vec<String> = s
                .safe()
                .prepare("SELECT name FROM pragma_table_info(?1)")
                .unwrap()
                .query_map([&t], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for c in cols {
                let c = c.to_lowercase();
                for bad in ["token", "password", "secret"] {
                    assert!(!c.contains(bad), "column {t}.{c} looks secret");
                }
            }
        }
    }

    fn st(n: u32) -> DiffStat {
        DiffStat {
            files: n,
            lines_added: n,
            lines_removed: 0,
            max_file_lines: n,
        }
    }

    #[test]
    fn wal_mode_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("cache.db")).unwrap();
        let pragma = |c: &Conn, name: &str| -> String {
            lock(c)
                .query_row(&format!("PRAGMA {name}"), [], |r| {
                    r.get::<_, rusqlite::types::Value>(0)
                })
                .map(|v| match v {
                    rusqlite::types::Value::Text(t) => t.to_lowercase(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    other => format!("{other:?}"),
                })
                .unwrap()
        };
        for c in [&s.safe, &s.fast, &s.read] {
            assert_eq!(pragma(c, "journal_mode"), "wal");
            assert_eq!(pragma(c, "busy_timeout"), "5000");
        }
        // R-store: drafts and markers are written with FULL (2), the cache with NORMAL (1)
        assert_eq!(pragma(&s.safe, "synchronous"), "2");
        assert_eq!(pragma(&s.fast, "synchronous"), "1");
        assert!(!Arc::ptr_eq(&s.safe, &s.fast) && !Arc::ptr_eq(&s.fast, &s.read));
        // the reader cannot write
        assert!(lock(&s.read).execute("DELETE FROM meta", []).is_err());
    }

    #[test]
    fn without_wal_the_store_falls_back_to_a_full_sync_rollback_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let no_wal = |_: &Connection| -> StoreResult<()> { Err(StoreError::WalUnavailable) };
        let msg = StoreError::WalUnavailable.to_string();
        assert!(
            msg.contains("WAL") && msg.contains("network drive"),
            "{msg}"
        );
        let s = Store::open_with(&path, no_wal).unwrap();
        assert!(s.wal_unavailable());
        let pragma = |c: &Conn, name: &str| -> String {
            lock(c)
                .query_row(&format!("PRAGMA {name}"), [], |r| {
                    r.get::<_, rusqlite::types::Value>(0)
                })
                .map(|v| match v {
                    rusqlite::types::Value::Text(t) => t.to_lowercase(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    other => format!("{other:?}"),
                })
                .unwrap()
        };
        for c in [&s.safe, &s.fast, &s.read] {
            assert_eq!(pragma(c, "journal_mode"), "delete");
        }
        // both writing connections are FULL: without WAL, NORMAL can corrupt on power loss
        assert_eq!(pragma(&s.safe, "synchronous"), "2");
        assert_eq!(pragma(&s.fast, "synchronous"), "2");
        let r = repo("acme", "api");
        let id = s.save_draft(&draft(&r, 1, "kept")).unwrap();
        s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
        s.set_selection(std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
        drop(s);
        let again = Store::open(&path).unwrap();
        assert!(!again.wal_unavailable());
        assert_eq!(again.list_drafts(&r, 1).unwrap()[0].id, id);
    }

    #[test]
    fn a_different_account_clears_the_rebuildable_tables_only() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let fill = |s: &Store| {
            s.upsert_repos(std::slice::from_ref(&r)).unwrap();
            s.apply_repo_sync(
                &r,
                Some(&[pr(&r, 1, PrState::Open)]),
                &[(1, "c".into(), st(1))],
                Some("e"),
            )
            .unwrap();
            s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
        };
        let draft_id = s.save_draft(&draft(&r, 1, "mine")).unwrap();
        // the first sign-in of a cache without an owner: whatever is there is not trusted
        fill(&s);
        assert!(s
            .adopt_user(&AccountId::default_bitbucket(), "{account-a}", "")
            .unwrap());
        assert!(s.list_prs().unwrap().is_empty());
        fill(&s);
        // the same account again keeps everything
        assert!(!s
            .adopt_user(&AccountId::default_bitbucket(), "{account-a}", "")
            .unwrap());
        assert_eq!(s.list_prs().unwrap().len(), 1);
        assert_eq!(
            s.cache_snapshot().unwrap().account_id.as_deref(),
            Some("{account-a}")
        );
        // another account: nothing of the first one stays readable, drafts are kept
        assert!(s
            .adopt_user(&AccountId::default_bitbucket(), "{account-b}", "")
            .unwrap());
        let snap = s.cache_snapshot().unwrap();
        assert!(snap.prs.is_empty() && snap.errors.is_empty() && snap.repo_count == 0);
        assert_eq!(snap.account_id.as_deref(), Some("{account-b}"));
        assert!(s.list_repos().unwrap().is_empty());
        assert_eq!(s.get_diffstat(&r, 1).unwrap(), None);
        assert!(s.viewed_map(&r, 1).unwrap().is_empty());
        assert_eq!(s.list_drafts(&r, 1).unwrap()[0].id, draft_id);
        assert_eq!(
            s.get_meta("account_id").unwrap().as_deref(),
            Some("{account-b}")
        );
    }

    #[test]
    fn rows_written_for_an_account_are_owned_by_it_and_purged_with_it() {
        let s = Store::open_in_memory().unwrap();
        let def = AccountId::default_bitbucket();
        let other = AccountId("acc-second".into());
        let (a, b) = (repo("acme", "api"), repo("beta", "web"));
        s.set_selection_for(&def, std::slice::from_ref(&a)).unwrap();
        s.set_selection_for(&other, std::slice::from_ref(&b))
            .unwrap();
        s.apply_repo_sync_for(&def, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
            .unwrap();
        s.apply_repo_sync_for(
            &other,
            &b,
            Some(&[pr(&b, 2, PrState::Open)]),
            &[(2, "c".into(), st(2))],
            Some("rate limited"),
        )
        .unwrap();
        s.set_repo_error_for(&other, &repo("beta", "*"), Some("access denied"))
            .unwrap();
        s.set_repo_error_for(&def, &repo("acme", "*"), Some("network error"))
            .unwrap();
        let snap = s.cache_snapshot().unwrap();
        assert_eq!(snap.prs.len(), 2, "both accounts' PRs are in the inbox");
        assert_eq!(snap.errors.len(), 3);
        let owner = |table: &str, key: &str| -> String {
            lock(&s.fast)
                .query_row(
                    &format!("SELECT account_id FROM {table} WHERE repo_key = ?1"),
                    params![key],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(owner("prs", &b.key()), "acc-second");
        assert_eq!(owner("diffstats", &b.key()), "acc-second");
        assert_eq!(owner("repo_errors", "acc-second:beta/*"), "acc-second");
        assert_eq!(owner("prs", &a.key()), "acc-bitbucket-default");
        // the repository moves to the other account (D8): its rows follow on the next sync
        s.set_selection_for(&other, &[a.clone(), b.clone()])
            .unwrap();
        s.apply_repo_sync_for(&other, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
            .unwrap();
        assert_eq!(owner("prs", &a.key()), "acc-second");
        let removed = s.remove_account(&other).unwrap();
        assert_eq!(
            (removed.prs, removed.diffstats, removed.repo_errors),
            (2, 1, 2)
        );
        let snap = s.cache_snapshot().unwrap();
        assert!(snap.prs.is_empty());
        assert_eq!(
            snap.errors,
            vec![(repo("acme", "*"), "network error".to_string())],
            "the other account's rows only"
        );
    }

    #[test]
    fn selection_of_every_account_in_one_save() {
        let s = Store::open_in_memory().unwrap();
        let (one, two) = (AccountId("acc-1".into()), AccountId("acc-2".into()));
        let (a, b, c) = (repo("acme", "api"), repo("acme", "web"), repo("beta", "x"));
        let listed = |r: &RepoRef| [RepoInfo::from(r.clone())];
        s.record_discovery_for(&one, &listed(&a), &[], None)
            .unwrap();
        s.record_discovery_for(&two, &listed(&a), &[], None)
            .unwrap();
        s.set_selection_for(&one, &[a.clone(), b.clone()]).unwrap();
        // a moves to account two, b is dropped, c is new under one; a listed twice keeps
        // its first account
        s.set_selection_all(&[
            (two.clone(), a.clone()),
            (one.clone(), c.clone()),
            (one.clone(), a.clone()),
        ])
        .unwrap();
        let mut got = s.selected_repos_by_account().unwrap();
        got.sort_by_key(|(acc, r)| (acc.clone(), r.key()));
        assert_eq!(got, vec![(one.clone(), c), (two.clone(), a.clone())]);
        let a_rows: Vec<(AccountId, bool)> = s
            .repo_choices()
            .unwrap()
            .into_iter()
            .filter(|ch| ch.repo == a)
            .map(|ch| (ch.account, ch.selected))
            .collect();
        assert_eq!(a_rows, vec![(one, false), (two, true)]);
    }

    #[test]
    fn repo_account_prefers_the_selecting_account() {
        let s = Store::open_in_memory().unwrap();
        let (one, two) = (AccountId("acc-1".into()), AccountId("acc-2".into()));
        let r = repo("acme", "api");
        assert_eq!(s.repo_account(&r).unwrap(), None);
        let listed = [RepoInfo::from(r.clone())];
        s.record_discovery_for(&one, &listed, &["acme".into()], None)
            .unwrap();
        s.record_discovery_for(&two, &listed, &["acme".into()], None)
            .unwrap();
        assert_eq!(s.repo_account(&r).unwrap(), Some(one.clone()));
        s.set_selection_for(&two, std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.repo_account(&r).unwrap(), Some(two));
    }

    #[test]
    fn adopting_a_user_touches_only_that_accounts_rows() {
        let s = Store::open_in_memory().unwrap();
        let def = AccountId::default_bitbucket();
        let other = AccountId("acc-second".into());
        let (a, b) = (repo("acme", "api"), repo("beta", "web"));
        s.set_selection_for(&def, std::slice::from_ref(&a)).unwrap();
        s.set_selection_for(&other, std::slice::from_ref(&b))
            .unwrap();
        assert!(s.adopt_user(&def, "{ana}", "Ana").unwrap());
        assert!(s.adopt_user(&other, "{bo}", "Bo").unwrap());
        s.apply_repo_sync_for(&def, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
            .unwrap();
        s.apply_repo_sync_for(&other, &b, Some(&[pr(&b, 2, PrState::Open)]), &[], None)
            .unwrap();
        // the other account signs in as someone else: only its rows go
        assert!(s.adopt_user(&other, "{cy}", "Cy").unwrap());
        let snap = s.cache_snapshot().unwrap();
        assert_eq!(snap.prs.len(), 1);
        assert_eq!(snap.prs[0].0.repo, a);
        assert_eq!(s.selected_repos().unwrap(), vec![a.clone()]);
        assert_eq!(snap.account_id.as_deref(), Some("{ana}"));
        let accounts = s.accounts().unwrap();
        let o = accounts.iter().find(|x| x.id == other).unwrap();
        assert_eq!(
            (o.user_id.as_str(), o.display_name.as_str()),
            ("{cy}", "Cy")
        );
        // the default account keeps its user: nothing changes
        assert!(!s.adopt_user(&def, "{ana}", "Ana").unwrap());
        assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
    }

    #[test]
    fn diffstats_alone_are_written_in_one_transaction() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        s.apply_repo_sync(&r, Some(&[pr(&r, 1, PrState::Open)]), &[], Some("e"))
            .unwrap();
        s.put_diffstats(&r, &[(1, "c1".into(), st(1)), (2, "c2".into(), st(2))])
            .unwrap();
        assert_eq!(s.get_diffstat(&r, 2).unwrap(), Some(("c2".into(), st(2))));
        // the PR list and the error row are not touched
        assert_eq!(s.list_prs().unwrap().len(), 1);
        assert_eq!(s.repo_errors().unwrap().len(), 1);
    }

    #[test]
    fn draft_marker_durable_with_two_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let r = repo("acme", "api");
        let s = Store::open(&path).unwrap();
        let id = s.save_draft(&draft(&r, 1, "send me")).unwrap();
        // cache writes on the other connection, before and after the marker
        s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
        s.mark_draft_posting(id).unwrap();
        s.put_diffstat(&r, 1, "c", &st(1)).unwrap();
        s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
        // a crash: no close, no checkpoint. The marker lives in the WAL only; copy the files
        // as they are on disk and open the copy.
        let wal = dir.path().join("cache.db-wal");
        assert!(
            std::fs::metadata(&wal).unwrap().len() > 0,
            "WAL holds the commits"
        );
        let copy = tempfile::tempdir().unwrap();
        std::fs::copy(&path, copy.path().join("cache.db")).unwrap();
        std::fs::copy(&wal, copy.path().join("cache.db-wal")).unwrap();
        std::mem::forget(s);
        let again = Store::open(&copy.path().join("cache.db")).unwrap();
        assert_eq!(again.posting_draft_ids(&r, 1).unwrap(), vec![id]);
        assert_eq!(again.list_drafts(&r, 1).unwrap()[0].text, "send me");
        assert_eq!(again.list_prs().unwrap().len(), 1);
    }

    #[test]
    fn replace_unchanged_list_does_not_write() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let prs = vec![pr(&r, 2, PrState::Open), pr(&r, 1, PrState::Merged)];
        let stats = vec![(2, "abc123".to_string(), st(4))];
        s.upsert_repos(std::slice::from_ref(&r)).unwrap();
        s.apply_repo_sync(&r, Some(&prs), &stats, Some("rate limited"))
            .unwrap();
        let before = s.fast_changes();
        // the same result again (in another order): nothing is rewritten
        let reordered = vec![prs[1].clone(), prs[0].clone()];
        s.apply_repo_sync(&r, Some(&reordered), &stats, Some("rate limited"))
            .unwrap();
        s.replace_prs(&r, &prs).unwrap();
        s.upsert_repos(std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.fast_changes(), before, "a no-change sync writes no rows");
        // a changed PR is written
        let mut moved = prs.clone();
        moved[0].source_commit = "new".into();
        s.replace_prs(&r, &moved).unwrap();
        assert!(s.fast_changes() > before);
        let stored = s.list_prs().unwrap();
        assert!(stored.iter().any(|p| p.source_commit == "new"));
    }

    #[test]
    fn apply_repo_sync_atomic() {
        let s = Store::open_in_memory().unwrap();
        let r = repo("acme", "api");
        let old = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)];
        s.apply_repo_sync(
            &r,
            Some(&old),
            &[(1, "c1".into(), st(1))],
            Some("old error"),
        )
        .unwrap();
        assert_eq!(s.list_prs().unwrap(), old);
        // the diffstats and the error row are written first, then the PRs fail (two rows
        // with one id): everything rolls back
        let bad = vec![pr(&r, 5, PrState::Open), pr(&r, 5, PrState::Merged)];
        assert!(s
            .apply_repo_sync(&r, Some(&bad), &[(1, "c2".into(), st(9))], None)
            .is_err());
        assert_eq!(s.list_prs().unwrap(), old);
        assert_eq!(s.get_diffstat(&r, 1).unwrap(), Some(("c1".into(), st(1))));
        assert_eq!(
            s.repo_errors().unwrap(),
            vec![(r.clone(), "old error".to_string())]
        );
        // without a PR list the stored PRs stay, the rest is written
        s.apply_repo_sync(&r, None, &[(2, "c3".into(), st(3))], None)
            .unwrap();
        assert_eq!(s.list_prs().unwrap(), old);
        assert_eq!(s.get_diffstat(&r, 2).unwrap(), Some(("c3".into(), st(3))));
        assert!(s.repo_errors().unwrap().is_empty());
        assert_eq!(
            s.diffstat_commits(&r).unwrap(),
            HashMap::from([(1, "c1".to_string()), (2, "c3".to_string())])
        );
    }

    #[test]
    fn cache_snapshot_equals_n_plus_one_reads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("cache.db")).unwrap();
        let (a, b) = (repo("acme", "api"), repo("acme", "web"));
        s.upsert_repos(&[a.clone(), b.clone(), repo("acme", "empty")])
            .unwrap();
        s.replace_prs(&b, &[pr(&b, 3, PrState::Open)]).unwrap();
        s.replace_prs(&a, &[pr(&a, 2, PrState::Open), pr(&a, 1, PrState::Merged)])
            .unwrap();
        s.put_diffstat(&a, 2, "x", &st(2)).unwrap();
        s.put_diffstat(&b, 3, "y", &st(3)).unwrap();
        // a diffstat of a PR that is gone is not part of the inbox
        s.put_diffstat(&b, 99, "z", &st(9)).unwrap();
        s.set_repo_error(&b, Some("http 500")).unwrap();
        s.set_selection(&[a.clone(), b.clone(), repo("acme", "empty")])
            .unwrap();
        let snap = s.cache_snapshot().unwrap();
        let old: Vec<(PullRequest, Option<(String, DiffStat)>)> = s
            .list_prs()
            .unwrap()
            .into_iter()
            .map(|p| {
                let d = s.get_diffstat(&p.repo, p.id).unwrap();
                (p, d)
            })
            .collect();
        assert_eq!(snap.prs, old);
        assert_eq!(snap.errors, s.repo_errors().unwrap());
        assert_eq!(snap.repo_count, s.list_repos().unwrap().len());
        assert_eq!(snap.prs.len(), 3);
    }

    #[test]
    fn cache_snapshot_reports_an_undecodable_pr() {
        let s = Store::open_in_memory().unwrap();
        s.safe()
            .execute(
                "INSERT INTO prs (repo_key, id, state, updated_on, json)
                 VALUES ('bitbucket.org/a/b', 1, 'OPEN', '', '{')",
                [],
            )
            .unwrap();
        s.set_selection(&[repo("a", "b")]).unwrap();
        assert!(s.cache_snapshot().is_err());
    }

    #[test]
    fn in_memory_store_still_works() {
        let s = Store::open_in_memory().unwrap();
        assert!(Arc::ptr_eq(&s.safe, &s.fast) && Arc::ptr_eq(&s.fast, &s.read));
        let r = repo("acme", "api");
        s.apply_repo_sync(
            &r,
            Some(&[pr(&r, 1, PrState::Open)]),
            &[(1, "c".into(), st(1))],
            None,
        )
        .unwrap();
        let id = s.save_draft(&draft(&r, 1, "x")).unwrap();
        s.mark_draft_posting(id).unwrap();
        s.set_viewed_many(&r, 1, &[("a.rs", "o", true)]).unwrap();
        s.save_settings(&Settings {
            clone_root: None,
            download_dir: PathBuf::from("d"),
        })
        .unwrap();
        s.set_selection(std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
        assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![id]);
        assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 1);
        s.checkpoint().unwrap();
    }

    #[test]
    fn concurrent_readers_and_writers_do_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(Store::open(&dir.path().join("cache.db")).unwrap());
        let all: Vec<RepoRef> = (0..6).map(|t| repo("w", &format!("r{t}"))).collect();
        s.set_selection(&all).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for t in 0..6u64 {
            let (s, done_tx) = (s.clone(), done_tx.clone());
            threads.push(std::thread::spawn(move || {
                let r = repo("w", &format!("r{t}"));
                for i in 0..40u64 {
                    match t % 3 {
                        0 => s
                            .apply_repo_sync(
                                &r,
                                Some(&[pr(&r, i % 5, PrState::Open)]),
                                &[(i, format!("c{i}"), st(i as u32))],
                                (i % 2 == 0).then_some("e"),
                            )
                            .unwrap(),
                        1 => {
                            let id = s.save_draft(&draft(&r, i, "d")).unwrap();
                            s.mark_draft_posting(id).unwrap();
                            s.set_viewed(&r, i, "a.rs", "o", true).unwrap();
                        }
                        _ => {
                            s.cache_snapshot().unwrap();
                            s.list_drafts(&r, i).unwrap();
                        }
                    }
                }
                let _ = done_tx.send(());
            }));
        }
        drop(done_tx);
        for _ in 0..6 {
            done_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("a store thread is stuck");
        }
        for t in threads {
            t.join().unwrap();
        }
        // two writer threads, each repo's list replaced by its last single-PR list
        assert_eq!(s.cache_snapshot().unwrap().prs.len(), 2);
    }

    // ---- v7: accounts and host-qualified keys ----

    /// A PR exactly as the v6 build serialized it (no host, `workspace`, `uuid`).
    fn v6_pr_json(ws: &str, slug: &str, id: u64) -> String {
        format!(
            r#"{{"repo":{{"workspace":"{ws}","slug":"{slug}"}},"id":{id},"title":"PR {id}",
            "state":"Open","draft":false,"author":{{"uuid":"{{a}}","display_name":"Al"}},
            "source_branch":"f{id}","dest_branch":"main","source_commit":"s{id}",
            "dest_commit":"d{id}","source_repo":{{"workspace":"{ws}","slug":"{slug}"}},
            "comment_count":0,"task_count":0,"reviewers":[],
            "created_on":"2026-01-01T00:00:00Z","updated_on":"2026-01-02T00:00:00Z"}}"#
        )
    }

    /// A v6 cache populated in every table, written with the v1..v6 schema exactly as the
    /// previous build left it.
    fn v6_cache(path: &Path) {
        let c = rusqlite::Connection::open(path).unwrap();
        for sql in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
        ] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 6).unwrap();
        c.execute_batch(
            "INSERT INTO meta (key, value) VALUES ('account_id', '{me}');
             INSERT INTO meta (key, value) VALUES ('me', '{me}\nAnn Example');
             INSERT INTO meta (key, value) VALUES ('clone_root', 'C:\\src');
             INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/api', 'acme', 'api');
             INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/web', 'acme', 'web');
             INSERT INTO repos (full_name, workspace, slug) VALUES ('Other/Mixed', 'Other', 'Mixed');
             INSERT INTO diffstats (repo, id, source_commit, json)
                 VALUES ('acme/api', 1, 's1',
                         '{\"files\":3,\"lines_added\":10,\"lines_removed\":2,\"max_file_lines\":7}');
             INSERT INTO repo_errors (full_name, workspace, slug, message)
                 VALUES ('*/*', '*', '*', 'network error');
             INSERT INTO repo_errors (full_name, workspace, slug, message)
                 VALUES ('bad/*', 'bad', '*', 'access denied');
             INSERT INTO repo_errors (full_name, workspace, slug, message)
                 VALUES ('acme/web', 'acme', 'web', 'not found');
             INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/api', 1, 'a.rs', 'o1');
             INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/api', 1, 'b.rs', 'o2');
             INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/web', 2, 'a.rs', 'o3');
             INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
                 VALUES ('acme/api', 1, 'pr', 'top level', 's1');
             INSERT INTO drafts (repo, pr_id, kind, parent_id, text, source_commit, comment_type)
                 VALUES ('acme/api', 1, 'reply', 77, 'a reply', 's1', 'nit');
             INSERT INTO drafts (repo, pr_id, kind, path, line, side, text, source_commit,
                                 posting_started_at)
                 VALUES ('acme/web', 2, 'inline', 'src/x.rs', 9, 'new', 'sent, unknown', 's2',
                         datetime('now'));
             INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posted_at)
                 VALUES ('acme/web', 2, 'pr', 'posted already', 's2', datetime('now'));
             DELETE FROM repo_choices;
             INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                       discovered_at, gone)
                 VALUES ('acme/api', 'acme', 'api', 'API', 1, 1, '2026-01-01 00:00:00', 0);
             INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                       discovered_at, gone)
                 VALUES ('acme/web', 'acme', 'web', 'Web', 0, 1, '2026-01-01 00:00:00', 0);
             INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                       discovered_at, gone)
                 VALUES ('acme/idle', 'acme', 'idle', NULL, NULL, 0, '2026-01-01 00:00:00', 0);
             INSERT INTO repo_choices (full_name, workspace, slug, selected, gone)
                 VALUES ('acme/gone', 'acme', 'gone', 1, 1);",
        )
        .unwrap();
        for (ws, slug, id) in [("acme", "api", 1), ("acme", "web", 2), ("acme", "idle", 3)] {
            c.execute(
                "INSERT INTO prs (repo, id, state, updated_on, json)
                 VALUES (?1, ?2, 'OPEN', '2026-01-02T00:00:00+00:00', ?3)",
                params![format!("{ws}/{slug}"), id as i64, v6_pr_json(ws, slug, id)],
            )
            .unwrap();
        }
    }

    #[test]
    fn v6_to_v7_rewrites_keys_and_keeps_drafts_viewed_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        v6_cache(&path);
        let s = Store::open(&path).unwrap();
        let v: i64 = s
            .safe()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let (api, web) = (repo("acme", "api"), repo("acme", "web"));
        let acc = AccountId::default_bitbucket();

        // the single sign-in became the default Bitbucket account (no secret in it)
        let accounts = s.accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, acc);
        assert_eq!(accounts[0].forge, Forge::BitbucketCloud);
        assert_eq!(accounts[0].git_host, "bitbucket.org");
        assert_eq!(accounts[0].user_id, "{me}");
        assert_eq!(accounts[0].display_name, "Ann Example");
        assert_eq!(
            s.get_meta("clone_root").unwrap().as_deref(),
            Some("C:\\src")
        );

        // drafts: every one kept with its text, kind, reply target, type and markers
        let ds = s.list_drafts(&api, 1).unwrap();
        assert_eq!(ds.len(), 2);
        assert_eq!(
            (ds[0].kind, ds[0].text.as_str()),
            (DraftKind::Pr, "top level")
        );
        assert_eq!(ds[1].kind, DraftKind::Reply);
        assert_eq!(ds[1].reply_to, Some(ThreadId::from(77)));
        assert_eq!(ds[1].comment_type, Some(CommentType::Nit));
        assert_eq!(ds[1].posted_text(), "**nit:** a reply");
        assert!(s.posted_draft_ids(&api, 1).unwrap().is_empty());
        let wd = s.list_drafts(&web, 2).unwrap();
        assert_eq!(wd.len(), 2);
        assert_eq!(
            (wd[0].path.as_deref(), wd[0].line, wd[0].side),
            (Some("src/x.rs"), Some(9), Some(Side::New))
        );
        assert_eq!(s.posting_draft_ids(&web, 2).unwrap(), vec![wd[0].id]);
        assert_eq!(s.posted_draft_ids(&web, 2).unwrap(), vec![wd[1].id]);

        // viewed marks per repo and PR, nothing mixed between them
        let va = s.viewed_map(&api, 1).unwrap();
        assert_eq!(va.len(), 2);
        assert_eq!(va["a.rs"], "o1");
        assert_eq!(s.viewed_map(&web, 2).unwrap()["a.rs"], "o3");
        assert!(s.viewed_map(&api, 2).unwrap().is_empty());
        assert!(s.viewed_map(&web, 1).unwrap().is_empty());

        // selection and discovery cache, owned by the default account
        let choices = s.repo_choices().unwrap();
        let summary: Vec<_> = choices
            .iter()
            .map(|c| {
                (
                    c.repo.full_name(),
                    c.name.clone(),
                    c.private,
                    c.selected,
                    c.gone,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "acme/api".into(),
                    Some("API".into()),
                    Some(true),
                    true,
                    false
                ),
                ("acme/gone".into(), None, None, true, true),
                ("acme/idle".into(), None, None, false, false),
                (
                    "acme/web".into(),
                    Some("Web".into()),
                    Some(false),
                    true,
                    false
                ),
            ]
        );
        assert!(choices.iter().all(|c| c.account == acc));
        assert!(choices.iter().all(|c| c.repo.host == "bitbucket.org"));
        assert_eq!(s.selected_repos().unwrap(), vec![api.clone(), web.clone()]);
        assert!(s.discovered_any().unwrap());

        // cached PRs decode (old JSON, serde defaults) and stay in their repos
        let prs = s.list_prs().unwrap();
        assert_eq!(prs.len(), 3);
        assert!(prs
            .iter()
            .all(|p| p.forge == Forge::BitbucketCloud && p.account == acc));
        assert_eq!(prs[0].repo, api);
        assert_eq!(prs[0].author.id, "{a}");
        let snap = s.cache_snapshot().unwrap();
        let ids: Vec<(String, u64)> = snap
            .prs
            .iter()
            .map(|(p, _)| (p.repo.full_name(), p.id))
            .collect();
        // acme/idle is not selected: cached, but not in the inbox
        assert_eq!(ids, vec![("acme/api".into(), 1), ("acme/web".into(), 2)]);
        assert_eq!(
            snap.prs[0].1.as_ref().map(|(c, d)| (c.as_str(), d.files)),
            Some(("s1", 3))
        );
        assert!(snap.prs[1].1.is_none(), "no diffstat moved to another PR");
        assert_eq!(snap.account_id.as_deref(), Some("{me}"));
        assert_eq!(snap.repo_count, 2);
        assert_eq!(snap.known_repos, 3);

        // errors: listing pseudo rows read back as before, the repo error kept
        assert_eq!(
            snap.errors,
            vec![
                (repo("*", "*"), "network error".to_string()),
                (repo("acme", "web"), "not found".to_string()),
                (repo("bad", "*"), "access denied".to_string()),
            ]
        );

        // the stored keys are the new shape, nothing left in the old one
        let keys = |sql: &str| -> Vec<String> {
            let c = s.safe();
            let mut st = c.prepare(sql).unwrap();
            let rows = st.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(
            keys("SELECT repo_key FROM repo_errors ORDER BY repo_key"),
            [
                "acc-bitbucket-default:*",
                "acc-bitbucket-default:bad/*",
                "bitbucket.org/acme/web"
            ]
        );
        assert_eq!(
            keys("SELECT DISTINCT repo_key FROM drafts ORDER BY repo_key"),
            ["bitbucket.org/acme/api", "bitbucket.org/acme/web"]
        );
        assert_eq!(
            keys("SELECT repo_key FROM repos ORDER BY repo_key"),
            [
                "bitbucket.org/acme/api",
                "bitbucket.org/acme/web",
                "bitbucket.org/other/mixed"
            ]
        );
        for table in [
            "prs",
            "diffstats",
            "viewed",
            "drafts",
            "repo_choices",
            "repos",
        ] {
            let odd = keys(&format!(
                "SELECT account_id FROM {table} WHERE account_id <> 'acc-bitbucket-default'"
            ));
            assert!(odd.is_empty(), "{table}: {odd:?}");
        }
        // the mixed-case repository keeps its display case
        assert!(s.list_repos().unwrap().contains(&repo("Other", "Mixed")));

        // and the upgraded cache keeps working
        s.set_viewed(&api, 1, "c.rs", "o9", true).unwrap();
        assert_eq!(s.viewed_map(&api, 1).unwrap().len(), 3);
        let mut d = ds[1].clone();
        d.text = "edited".into();
        assert_eq!(s.save_draft(&d).unwrap(), d.id);
        assert_eq!(
            s.list_drafts(&api, 1).unwrap()[1].reply_to,
            Some(ThreadId::from(77))
        );
        drop(s);
        // reopening does not migrate again
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_drafts(&api, 1).unwrap().len(), 2);
        assert_eq!(s.accounts().unwrap().len(), 1);
    }

    #[test]
    fn v6_cache_without_sign_in_gets_no_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            for sql in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
            ] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 6).unwrap();
            c.execute(
                "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
                 VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert!(s.accounts().unwrap().is_empty());
        assert_eq!(s.list_drafts(&repo("acme", "api"), 1).unwrap().len(), 1);
    }

    #[test]
    fn v7_refuses_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 9).unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema {
                found: 9,
                supported: 8
            })
        ));
    }

    /// v8: drafts gain the first line (and its side) of a multi-line comment; existing
    /// drafts are single-line.
    #[test]
    fn v7_drafts_gain_start_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        {
            let mut c = rusqlite::Connection::open(&path).unwrap();
            for sql in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
            ] {
                c.execute_batch(sql).unwrap();
            }
            let tx = c.transaction().unwrap();
            migrate_v7(&tx).unwrap();
            tx.pragma_update(None, "user_version", 7).unwrap();
            tx.commit().unwrap();
            c.execute(
                "INSERT INTO drafts (repo_key, pr_id, kind, path, line, side, text, source_commit)
                 VALUES ('bitbucket.org/acme/api', 1, 'inline', 'a.rs', 3, 'new', 'kept', 'h')",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let r = repo("acme", "api");
        let ds = s.list_drafts(&r, 1).unwrap();
        assert_eq!((ds.len(), ds[0].start), (1, None));
        let mut d = ds[0].clone();
        d.start = Some((Side::Old, 2));
        s.save_draft(&d).unwrap();
        let mut n = draft(&r, 1, "range");
        n.start = Some((Side::New, 10));
        s.save_draft(&n).unwrap();
        let starts: Vec<_> = s
            .list_drafts(&r, 1)
            .unwrap()
            .iter()
            .map(|d| d.start)
            .collect();
        assert_eq!(starts, [Some((Side::Old, 2)), Some((Side::New, 10))]);
        // a start with an unknown side is skipped like any corrupt row
        s.safe()
            .execute(
                "INSERT INTO drafts (repo_key, pr_id, kind, line, side, start_side, start_line,
                                     text, source_commit)
                 VALUES ('bitbucket.org/acme/api', 1, 'inline', 4, 'new', 'up', 1, 't', 'c')",
                [],
            )
            .unwrap();
        assert_eq!(s.list_drafts(&r, 1).unwrap().len(), 2);
    }

    fn account(id: &str, forge: Forge, host: &str) -> Account {
        Account {
            id: AccountId(id.into()),
            forge,
            web_base: format!("https://{host}"),
            api_base: format!("https://api.{host}"),
            graphql_base: None,
            git_host: host.into(),
            user_id: "1".into(),
            login: "me".into(),
            display_name: "Me".into(),
            auth_kind: "pat".into(),
            enabled: true,
        }
    }

    #[test]
    fn accounts_round_trip_and_enable() {
        let s = Store::open_in_memory().unwrap();
        let a = account("acc-gh", Forge::GitHub, "github.com");
        s.upsert_account(&a).unwrap();
        assert_eq!(s.accounts().unwrap(), vec![a.clone()]);
        s.set_account_enabled(&a.id, false).unwrap();
        assert!(!s.accounts().unwrap()[0].enabled);
        assert!(matches!(
            s.set_account_enabled(&AccountId("nope".into()), true),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn same_repo_two_accounts_select_one_only() {
        let s = Store::open_in_memory().unwrap();
        let (a, b) = (AccountId("acc-a".into()), AccountId("acc-b".into()));
        let r = RepoRef::parse("github.com", "org/tool").unwrap();
        let info = [RepoInfo::from(r.clone())];
        s.record_discovery_for(&a, &info, &["org".into()], None)
            .unwrap();
        s.record_discovery_for(&b, &info, &["org".into()], None)
            .unwrap();
        assert_eq!(s.repo_choices().unwrap().len(), 2, "listed under both");
        s.set_selection_for(&a, std::slice::from_ref(&r)).unwrap();
        assert_eq!(
            s.selected_repos_by_account().unwrap(),
            vec![(a.clone(), r.clone())]
        );
        s.set_selection_for(&b, std::slice::from_ref(&r)).unwrap();
        assert_eq!(
            s.selected_repos_by_account().unwrap(),
            vec![(b.clone(), r.clone())]
        );
        // a's own selection call never touches b's other choices
        s.set_selection_for(&a, &[]).unwrap();
        assert_eq!(s.selected_repos_by_account().unwrap(), vec![(b, r)]);
    }

    #[test]
    fn remove_account_purges_its_rows_keeps_drafts_marked() {
        let s = Store::open_in_memory().unwrap();
        let gone = account("acc-gl", Forge::GitLab, "gitlab.com");
        s.upsert_account(&gone).unwrap();
        let keep = Account::default_bitbucket("{me}".into(), "Me".into());
        s.upsert_account(&keep).unwrap();
        let mine = RepoRef::parse("gitlab.com", "grp/sub/proj").unwrap();
        let other = repo("acme", "api");
        s.record_discovery_for(&gone.id, &[RepoInfo::from(mine.clone())], &[], None)
            .unwrap();
        s.set_selection_for(&gone.id, std::slice::from_ref(&mine))
            .unwrap();
        s.set_selection(std::slice::from_ref(&other)).unwrap();
        let mut p = pr(&mine, 4, PrState::Open);
        p.account = gone.id.clone();
        s.replace_prs(&mine, &[p]).unwrap();
        s.replace_prs(&other, &[pr(&other, 1, PrState::Open)])
            .unwrap();
        // rows written for the removed account carry its id
        s.safe()
            .execute(
                "UPDATE prs SET account_id = 'acc-gl' WHERE repo_key = ?1",
                params![mine.key()],
            )
            .unwrap();
        s.set_sync_cursor(&gone.id, &mine, "open", &SyncCursorRow::default())
            .unwrap();
        let mut d = draft(&mine, 4, "keep me");
        d.kind = DraftKind::Pr;
        s.save_draft(&d).unwrap();
        s.safe()
            .execute("UPDATE drafts SET account_id = 'acc-gl'", [])
            .unwrap();

        let n = s.remove_account(&gone.id).unwrap();
        assert_eq!(
            n,
            RemovedCounts {
                prs: 1,
                diffstats: 0,
                repo_choices: 1,
                repo_errors: 0,
                cursors: 1,
                drafts_kept: 1,
            }
        );
        assert_eq!(s.accounts().unwrap(), vec![keep]);
        assert_eq!(s.selected_repos().unwrap(), vec![other.clone()]);
        assert_eq!(
            s.list_prs().unwrap().len(),
            1,
            "the other account's PR stays"
        );
        assert_eq!(s.list_drafts(&mine, 4).unwrap().len(), 1, "drafts are kept");
        assert!(s.sync_cursor(&gone.id, &mine, "open").unwrap().is_none());
    }

    #[test]
    fn nested_namespace_round_trips() {
        let s = Store::open_in_memory().unwrap();
        let acc = AccountId("acc-gl".into());
        let r = RepoRef::parse("git.corp.example/gitlab", "Group/Sub/Proj").unwrap();
        s.record_discovery_for(&acc, &[RepoInfo::from(r.clone())], &[], None)
            .unwrap();
        s.set_selection_for(&acc, std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.selected_repos().unwrap(), vec![r.clone()]);
        assert_eq!(s.repo_choices().unwrap()[0].repo, r);
        s.upsert_repos(std::slice::from_ref(&r)).unwrap();
        assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
        s.set_viewed(&r, 3, "x", "o", true).unwrap();
        assert_eq!(s.viewed_map(&r, 3).unwrap().len(), 1);
        // the same path on another host is another repository
        let bb = repo("Group/Sub", "Proj");
        assert!(s.viewed_map(&bb, 3).unwrap().is_empty());
    }

    #[test]
    fn cursor_set_get() {
        let s = Store::open_in_memory().unwrap();
        let acc = AccountId("acc-gh".into());
        let r = RepoRef::parse("github.com", "o/r").unwrap();
        assert!(s.sync_cursor(&acc, &r, "open").unwrap().is_none());
        let c = SyncCursorRow {
            etag: Some("W/\"abc\"".into()),
            last_updated: Some("2026-11-13T10:00:00Z".into()),
        };
        s.set_sync_cursor(&acc, &r, "open", &c).unwrap();
        assert_eq!(s.sync_cursor(&acc, &r, "open").unwrap(), Some(c.clone()));
        assert!(s.sync_cursor(&acc, &r, "closed").unwrap().is_none());
        let other = AccountId("acc-other".into());
        assert!(s.sync_cursor(&other, &r, "open").unwrap().is_none());
        let c2 = SyncCursorRow {
            etag: None,
            last_updated: Some("2026-11-14T00:00:00Z".into()),
        };
        s.set_sync_cursor(&acc, &r, "open", &c2).unwrap();
        assert_eq!(s.sync_cursor(&acc, &r, "open").unwrap(), Some(c2));
    }
}
