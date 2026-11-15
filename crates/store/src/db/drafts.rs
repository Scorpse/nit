use super::*;

pub(super) fn kind_str(k: DraftKind) -> &'static str {
    match k {
        DraftKind::Pr => "pr",
        DraftKind::File => "file",
        DraftKind::Inline => "inline",
        DraftKind::Reply => "reply",
    }
}

pub(super) fn parse_kind(s: &str) -> Option<DraftKind> {
    Some(match s {
        "pr" => DraftKind::Pr,
        "file" => DraftKind::File,
        "inline" => DraftKind::Inline,
        "reply" => DraftKind::Reply,
        _ => return None,
    })
}

pub(super) fn side_str(s: Side) -> &'static str {
    match s {
        Side::Old => "old",
        Side::New => "new",
    }
}

pub(super) fn parse_side(s: &str) -> Option<Side> {
    match s {
        "old" => Some(Side::Old),
        "new" => Some(Side::New),
        _ => None,
    }
}

impl Store {
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

pub(super) fn set_viewed_in(
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
