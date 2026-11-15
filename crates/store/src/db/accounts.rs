use super::*;

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

pub(super) fn upsert_account_in(conn: &Connection, a: &Account) -> StoreResult<()> {
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
