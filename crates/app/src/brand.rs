//! The program's name, `nit`, and the carry-over from its old name `prview`. The old name is
//! read only to bring an existing install along: the data folder is renamed, the Credential
//! Manager entries are moved, and the old environment variables are still accepted
//! (deprecated). Nothing is ever written under the old name.

use std::path::{Component, Path, PathBuf};

use store::{
    SecretStore, Settings, Store, LEGACY_BB_EMAIL as KEY_BB_EMAIL, LEGACY_BB_TOKEN as KEY_BB_TOKEN,
};

/// Folder, Credential Manager service, window title and executable stem.
pub const NAME: &str = "nit";
/// The name before the rename; only read to migrate an old install.
pub const LEGACY_NAME: &str = "prview";

/// The platform families whose per-user data folder differs (see `data_base_for`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum Platform {
    Windows,
    MacOs,
    /// Linux and the other Unix systems: the XDG base directory layout.
    Xdg,
}

const PLATFORM: Platform = if cfg!(windows) {
    Platform::Windows
} else if cfg!(target_os = "macos") {
    Platform::MacOs
} else {
    Platform::Xdg
};

/// What is missing when there is no data folder (shown in the UI and in errors).
pub const DATA_DIR_UNSET: &str = if cfg!(windows) {
    "LOCALAPPDATA is not set"
} else {
    "HOME is not set"
};

/// The per-user folder the data folder lives in, from the environment `env`: Windows
/// `%LOCALAPPDATA%`; macOS `$HOME/Library/Application Support`; elsewhere `$XDG_DATA_HOME`,
/// else `$HOME/.local/share`. On macOS and Unix only absolute values count (the XDG spec
/// says a relative one is ignored), so the current directory never becomes the data folder.
/// `None` when the variable it needs is unset.
fn data_base_for(
    platform: Platform,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let absolute = |k: &str| {
        env(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() || p.has_root())
    };
    match platform {
        Platform::Windows => env("LOCALAPPDATA").map(PathBuf::from),
        Platform::MacOs => absolute("HOME").map(|h| h.join("Library").join("Application Support")),
        Platform::Xdg => absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|h| h.join(".local").join("share"))),
    }
}

/// The per-user folder the data folder lives in on this platform (see `data_base_for`).
pub fn data_base() -> Option<PathBuf> {
    data_base_for(PLATFORM, &|k| std::env::var_os(k))
}

/// The data folder under `base` on this platform: `data_dir_in` on Windows (which knows the
/// old `prview` folder), `base/nit` elsewhere (there never was a `prview` there).
pub fn data_dir_at(base: &Path) -> PathBuf {
    if PLATFORM == Platform::Windows {
        data_dir_in(base)
    } else {
        base.join(NAME)
    }
}

/// The data folder under `base`: `base\nit`, or `base\prview` while only the old one exists
/// (before the move, or after a move that failed). Never creates or moves anything, so the
/// demo, the token check and the crash log never move data on their own.
pub fn data_dir_in(base: &Path) -> PathBuf {
    let new = base.join(NAME);
    let old = base.join(LEGACY_NAME);
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

/// The data folder (crash log, cache, mirrors, token-check report): Windows
/// `%LOCALAPPDATA%\nit` (see `data_dir_in`), macOS `~/Library/Application Support/nit`,
/// Linux `$XDG_DATA_HOME/nit` or `~/.local/share/nit`. `None` when the variable it needs is
/// unset (`DATA_DIR_UNSET`).
pub fn data_dir() -> Option<PathBuf> {
    data_base().map(|b| data_dir_at(&b))
}

/// What `migrate_data_dir_in` did.
#[derive(Debug, PartialEq, Eq)]
pub enum DirMove {
    /// Nothing to move: no old folder, or the new one already exists.
    Nothing,
    Moved,
    /// The rename failed (a file held open, no permission): nothing changed, the old folder
    /// stays in use and the next start tries again.
    Failed(String),
}

/// Renames `base\prview` to `base\nit` when only the old one exists. A rename on one volume
/// moves everything at once or nothing, so the cache, mirrors and drafts are never half
/// moved; on failure `data_dir_in` keeps returning the old folder.
pub fn migrate_data_dir_in(base: &Path) -> DirMove {
    let new = base.join(NAME);
    let old = base.join(LEGACY_NAME);
    if new.exists() || !old.is_dir() {
        return DirMove::Nothing;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => DirMove::Moved,
        Err(e) => DirMove::Failed(e.to_string()),
    }
}

/// Runs once at start-up, before the crash log and the store pick their folder, and only in
/// the real app (after the single-instance guard). A failure is noted in the crash log.
/// Windows only: `prview` never ran anywhere else, so there is nothing to move.
pub fn migrate_data_dir() {
    if PLATFORM != Platform::Windows {
        return;
    }
    let Some(base) = data_base() else {
        return;
    };
    if let DirMove::Failed(e) = migrate_data_dir_in(&base) {
        crate::crashlog::record(
            "the data folder %LOCALAPPDATA%\\prview could not be renamed to nit; it stays in use",
            &e,
        );
    }
}

/// `path` with its leading `prefix` removed, comparing components without ASCII case (the
/// Windows file system ignores it); `None` when `path` is not under `prefix`.
fn strip_prefix_ci<'a>(path: &'a Path, prefix: &Path) -> Option<&'a Path> {
    let mut rest = path.components();
    for want in prefix.components() {
        let got = rest.next()?;
        let same = match (got, want) {
            (Component::Normal(a), Component::Normal(b)) => a
                .to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy()),
            (a, b) => a == b,
        };
        if !same {
            return None;
        }
    }
    Some(rest.as_path())
}

/// A stored folder that pointed into `base\prview`, pointed at the same place under
/// `base\nit`; `None` when it is elsewhere or the old folder still exists (then it is
/// still valid).
pub fn rebased(path: &Path, base: &Path) -> Option<PathBuf> {
    let old = base.join(LEGACY_NAME);
    if old.exists() {
        return None;
    }
    strip_prefix_ci(path, &old).map(|rest| base.join(NAME).join(rest))
}

/// Settings saved before the move can hold `%LOCALAPPDATA%\prview\mirrors` (the old default
/// download folder, saved as a full path): they are pointed at the moved folder. Best effort;
/// a failure leaves them as they were (the Settings screen shows and fixes the path).
pub fn rebase_settings(store: &Store, base: &Path, default_download: &Path) {
    let Ok(s) = store.settings(default_download) else {
        return;
    };
    let download_dir = rebased(&s.download_dir, base);
    let clone_root = s.clone_root.as_deref().and_then(|p| rebased(p, base));
    if download_dir.is_none() && clone_root.is_none() {
        return;
    }
    let _ = store.save_settings(&Settings {
        download_dir: download_dir.unwrap_or(s.download_dir),
        clone_root: clone_root.or(s.clone_root),
    });
}

/// What `migrate_credentials` did. Carries no values.
#[derive(Debug, PartialEq, Eq)]
pub enum CredMove {
    /// Nothing to move: the new service already has an entry (or cannot be read), or the old
    /// one has no complete email and token.
    Nothing,
    Moved,
    /// Writing under the new name failed: what was written is removed again and the old
    /// entries are kept, so the next start tries again.
    Failed,
}

/// Copies the saved email and token from the old Credential Manager service (`prview`) to the
/// new one (`nit`) when the new one has neither, then deletes the old entries. The old ones
/// are deleted only after both were written. Values are never logged or returned.
pub fn migrate_credentials(new: &dyn SecretStore, old: &dyn SecretStore) -> CredMove {
    match (new.get(KEY_BB_EMAIL), new.get(KEY_BB_TOKEN)) {
        (Ok(None), Ok(None)) => {}
        _ => return CredMove::Nothing,
    }
    let (Ok(Some(email)), Ok(Some(token))) = (old.get(KEY_BB_EMAIL), old.get(KEY_BB_TOKEN)) else {
        return CredMove::Nothing;
    };
    let copied = new
        .set(KEY_BB_EMAIL, &email)
        .and_then(|_| new.set(KEY_BB_TOKEN, &token));
    if copied.is_err() {
        let _ = new.delete(KEY_BB_TOKEN);
        let _ = new.delete(KEY_BB_EMAIL);
        return CredMove::Failed;
    }
    let _ = old.delete(KEY_BB_TOKEN);
    let _ = old.delete(KEY_BB_EMAIL);
    CredMove::Moved
}

/// The value of the environment variable `name`, else of its deprecated old name `legacy`
/// (blank counts as unset).
pub fn env_or_legacy(
    env: &impl Fn(&str) -> Option<String>,
    name: &str,
    legacy: &str,
) -> Option<String> {
    let set = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    set(name).or_else(|| set(legacy))
}

#[cfg(test)]
mod tests;
