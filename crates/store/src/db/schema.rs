use super::*;

/// Schema version this build understands (stored in `PRAGMA user_version`).
pub(super) const SCHEMA_VERSION: i64 = 8;

pub(super) const SCHEMA_V1: &str = "
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

pub(super) const SCHEMA_V2: &str = "
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
pub(super) const SCHEMA_V3: &str = "ALTER TABLE drafts ADD COLUMN posted_at TEXT;";

/// v4: set right before a draft is sent; a draft with it and without `posted_at` has an
/// unknown outcome (a crash or a lost answer) and is never offered for posting silently.
pub(super) const SCHEMA_V4: &str = "ALTER TABLE drafts ADD COLUMN posting_started_at TEXT;";

/// v5: the repositories a discovery listed (a cache) and the ones the user chose to sync.
/// `gone`: a later discovery of its workspace no longer listed it (kept only while selected).
/// A cache that already holds PRs keeps syncing their repositories: they are selected once,
/// here, and the count is left in meta for a one-time notice.
pub(super) const SCHEMA_V5: &str = "
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
pub(super) const SCHEMA_V6: &str = "ALTER TABLE drafts ADD COLUMN comment_type TEXT;";

/// v7 (multi-forge): accounts, sync cursors, and host-qualified repo keys
/// (`bitbucket.org/ws/slug`, lower-case: `RepoRef::key`) in every repo column, renamed
/// `repo_key` (and `workspace` to `namespace`). Account-owned rows gain `account_id`; every
/// existing row belongs to the single Bitbucket sign-in, `DEFAULT_BITBUCKET_ACCOUNT`. Drafts
/// gain `reply_to` (the thread id as text; `parent_id` is kept, unused) and `forge`.
/// `repo_choices` is rebuilt to key on `(account_id, repo_key)`. The key rewrite is done by
/// `migrate_v7` in Rust, in the same transaction.
pub(super) const SCHEMA_V7: &str = "
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
pub(super) const SCHEMA_V8: &str = "
ALTER TABLE drafts ADD COLUMN start_side TEXT;
ALTER TABLE drafts ADD COLUMN start_line INTEGER;
";

/// Creates or upgrades the schema (additive steps only).
pub(super) fn migrate(conn: &Connection) -> StoreResult<()> {
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
pub(super) fn v7_key(old: &str) -> String {
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
pub(super) fn migrate_v7(tx: &Transaction) -> StoreResult<()> {
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
pub(super) type ChoiceRowV6 = (
    String,
    String,
    String,
    Option<String>,
    Option<bool>,
    bool,
    Option<String>,
    bool,
);
