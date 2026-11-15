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

mod accounts;
mod conn;
mod drafts;
mod prs;
mod repos;
mod schema;
mod settings;
mod types;

#[cfg(test)]
mod tests;

pub use prs::StoreReader;
pub use settings::META_REPOS_PRESELECTED;
pub use types::*;

use accounts::*;
use schema::*;
use settings::*;

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
