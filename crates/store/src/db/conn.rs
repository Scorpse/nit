use super::*;

pub(super) const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Every statement of the store fits, also on the single in-memory connection.
pub(super) const STATEMENT_CACHE: usize = 64;

/// Switches `conn` to WAL; `WalUnavailable` when the file system does not allow it.
pub(super) fn enable_wal(conn: &Connection) -> StoreResult<()> {
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
pub(super) enum Journal {
    Wal,
    Delete,
}

pub(super) fn open_file(
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
}
