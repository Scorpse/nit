use super::*;

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
