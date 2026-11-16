use super::*;

impl Store {
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

    /// Every meta entry whose key starts with `prefix` (which holds no `%`, `_` or `\`).
    pub fn meta_prefix(
        &self,
        prefix: &str,
    ) -> StoreResult<std::collections::HashMap<String, String>> {
        let conn = lock(&self.read);
        let mut stmt = conn.prepare_cached("SELECT key, value FROM meta WHERE key LIKE ?1")?;
        let rows = stmt.query_map([format!("{prefix}%")], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

pub(super) fn cache_snapshot_in(read: &Conn) -> StoreResult<CacheSnapshot> {
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
}

/// (id, state, updated_on, json) as stored for a PR.
pub(super) type PrRow = (i64, String, String, String);

pub(super) fn pr_row(pr: &PullRequest) -> StoreResult<PrRow> {
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
pub(super) fn replace_prs_in(
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

pub(super) fn put_diffstat_in(
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

pub(super) fn set_repo_error_in(
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

pub(super) fn repo_errors_in(conn: &Connection) -> StoreResult<Vec<(RepoRef, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT host, namespace, slug, message FROM repo_errors
         ORDER BY namespace || '/' || slug",
    )?;
    let rows = stmt.query_map([], |r| Ok((repo_at(0)(r)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
