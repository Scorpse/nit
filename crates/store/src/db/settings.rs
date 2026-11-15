use super::*;

pub(super) const META_CLONE_ROOT: &str = "clone_root";
pub(super) const META_DOWNLOAD_DIR: &str = "download_dir";
/// The user the default account was last signed in as (see `Store::adopt_user`).
pub(super) const META_ACCOUNT_ID: &str = "account_id";
/// How many repositories the v5 migration selected because the cache already held their PRs;
/// deleted when the user dismisses the notice about it.
pub const META_REPOS_PRESELECTED: &str = "repos_preselected";

impl Store {
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
}

pub(super) fn set_meta_in(conn: &Connection, key: &str, value: &str) -> StoreResult<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
    )?
    .execute(params![key, value])?;
    Ok(())
}

pub(super) fn delete_meta_in(conn: &Connection, key: &str) -> StoreResult<()> {
    conn.prepare_cached("DELETE FROM meta WHERE key = ?1")?
        .execute(params![key])?;
    Ok(())
}

pub(super) fn get_meta_in(conn: &Connection, key: &str) -> StoreResult<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row(params![key], |r| r.get(0))
        .optional()?)
}
