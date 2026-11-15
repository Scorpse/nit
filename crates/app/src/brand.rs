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
mod tests {
    use super::*;
    use store::{MemorySecrets, SecretError};

    const EMAIL: &str = "ana@example.com";
    const TOKEN: &str = "ATATT-not-a-real-token-value";

    fn env_of(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.into())
    }

    #[test]
    fn the_data_base_follows_each_platforms_convention() {
        let all = env_of(&[
            ("LOCALAPPDATA", r"C:\Users\ana\AppData\Local"),
            ("HOME", "/home/ana"),
            ("XDG_DATA_HOME", "/data/ana"),
        ]);
        assert_eq!(
            data_base_for(Platform::Windows, &all),
            Some(PathBuf::from(r"C:\Users\ana\AppData\Local"))
        );
        assert_eq!(
            data_base_for(Platform::MacOs, &all),
            Some(
                Path::new("/home/ana")
                    .join("Library")
                    .join("Application Support")
            )
        );
        assert_eq!(
            data_base_for(Platform::Xdg, &all),
            Some(PathBuf::from("/data/ana"))
        );
        // XDG_DATA_HOME unset, empty or relative: ~/.local/share
        let home = Path::new("/home/ana").join(".local").join("share");
        for env in [
            env_of(&[("HOME", "/home/ana")]),
            env_of(&[("HOME", "/home/ana"), ("XDG_DATA_HOME", "")]),
            env_of(&[("HOME", "/home/ana"), ("XDG_DATA_HOME", "rel/dir")]),
        ] {
            assert_eq!(data_base_for(Platform::Xdg, &env), Some(home.clone()));
        }
        // nothing usable: no data folder (never the current directory)
        let none = env_of(&[]);
        let relative_home = env_of(&[("HOME", "ana")]);
        for p in [Platform::Windows, Platform::MacOs, Platform::Xdg] {
            assert_eq!(data_base_for(p, &none), None, "{p:?}");
        }
        assert_eq!(data_base_for(Platform::MacOs, &relative_home), None);
        assert_eq!(data_base_for(Platform::Xdg, &relative_home), None);
        // the other platforms never read LOCALAPPDATA, Windows never reads HOME
        let win_only = env_of(&[("LOCALAPPDATA", r"C:\x")]);
        assert_eq!(data_base_for(Platform::Xdg, &win_only), None);
        assert_eq!(data_base_for(Platform::Windows, &relative_home), None);
    }

    #[test]
    fn the_data_folder_is_named_nit_and_only_windows_knows_prview() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("prview")).unwrap();
        let want = if cfg!(windows) { "prview" } else { "nit" };
        assert_eq!(data_dir_at(base.path()), base.path().join(want));
        let fresh = tempfile::tempdir().unwrap();
        assert_eq!(data_dir_at(fresh.path()), fresh.path().join("nit"));
    }

    #[test]
    fn a_fresh_machine_uses_the_new_folder_and_moves_nothing() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Nothing);
        assert_eq!(data_dir_in(base.path()), base.path().join("nit"));
        assert!(!base.path().join("nit").exists(), "nothing is created");
    }

    #[test]
    fn the_old_folder_is_renamed_with_everything_in_it() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("prview");
        std::fs::create_dir_all(old.join("mirrors/acme/api.git")).unwrap();
        std::fs::write(old.join("cache.sqlite"), b"cache").unwrap();
        std::fs::write(old.join("mirrors/acme/api.git/HEAD"), b"ref").unwrap();
        // before the move the old folder is the one in use
        assert_eq!(data_dir_in(base.path()), old);
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Moved);
        let new = base.path().join("nit");
        assert_eq!(data_dir_in(base.path()), new);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new.join("cache.sqlite")).unwrap(), b"cache");
        assert_eq!(
            std::fs::read(new.join("mirrors/acme/api.git/HEAD")).unwrap(),
            b"ref"
        );
        // a second start has nothing left to do
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Nothing);
    }

    #[test]
    fn an_existing_new_folder_wins_and_the_old_one_is_left_alone() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("prview")).unwrap();
        std::fs::write(base.path().join("prview/cache.sqlite"), b"old").unwrap();
        std::fs::create_dir_all(base.path().join("nit")).unwrap();
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Nothing);
        assert_eq!(data_dir_in(base.path()), base.path().join("nit"));
        assert_eq!(
            std::fs::read(base.path().join("prview/cache.sqlite")).unwrap(),
            b"old"
        );
    }

    /// What happens when an old prview (or a git process) still holds a file open.
    #[cfg(windows)]
    #[test]
    fn a_failed_rename_keeps_using_the_old_folder() {
        use std::os::windows::fs::OpenOptionsExt;
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("prview");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("cache.sqlite"), b"cache").unwrap();
        // a file open without delete sharing blocks renaming its folder
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ only
            .open(old.join("cache.sqlite"))
            .unwrap();
        assert!(matches!(
            migrate_data_dir_in(base.path()),
            DirMove::Failed(_)
        ));
        assert_eq!(data_dir_in(base.path()), old);
        assert!(!base.path().join("nit").exists());
        drop(held);
        assert_eq!(std::fs::read(old.join("cache.sqlite")).unwrap(), b"cache");
        // the next start moves it
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Moved);
    }

    /// Real Windows paths; `stored_folders_follow_the_move_on_any_path_syntax` checks the
    /// same with paths built for the platform the tests run on.
    #[cfg(windows)]
    #[test]
    fn stored_folders_under_the_old_data_folder_follow_the_move() {
        let base = Path::new(r"C:\Users\ana\AppData\Local");
        // the base does not exist here, so neither does its old folder
        assert_eq!(
            rebased(
                Path::new(r"C:\Users\ana\AppData\Local\prview\mirrors"),
                base
            ),
            Some(PathBuf::from(r"C:\Users\ana\AppData\Local\nit\mirrors"))
        );
        assert_eq!(
            rebased(
                Path::new(r"c:\users\ana\appdata\local\PRVIEW\mirrors"),
                base
            ),
            Some(PathBuf::from(r"C:\Users\ana\AppData\Local\nit\mirrors"))
        );
        assert_eq!(rebased(Path::new(r"D:\prview\mirrors"), base), None);
        assert_eq!(
            rebased(Path::new(r"C:\Users\ana\AppData\Local\prview-x"), base),
            None
        );
    }

    #[test]
    fn stored_folders_follow_the_move_on_any_path_syntax() {
        let root = |parts: &[&str]| -> PathBuf {
            parts
                .iter()
                .fold(PathBuf::from("no-such-root-for-nit-tests"), |p, s| {
                    p.join(s)
                })
        };
        let base = root(&["Users", "ana", "AppData", "Local"]);
        assert_eq!(
            rebased(&base.join("prview").join("mirrors"), &base),
            Some(base.join("nit").join("mirrors"))
        );
        // components compare without ASCII case
        assert_eq!(
            rebased(
                &root(&["users", "ana", "appdata", "local", "PRVIEW", "mirrors"]),
                &base
            ),
            Some(base.join("nit").join("mirrors"))
        );
        assert_eq!(rebased(&root(&["prview", "mirrors"]), &base), None);
        assert_eq!(rebased(&base.join("prview-x"), &base), None);
    }

    #[test]
    fn saved_settings_are_rebased_only_after_the_old_folder_is_gone() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("prview");
        std::fs::create_dir_all(&old).unwrap();
        let store = Store::open_in_memory().unwrap();
        let default = base.path().join("nit").join("mirrors");
        let saved = Settings {
            clone_root: Some(PathBuf::from(r"D:\code")),
            download_dir: old.join("mirrors"),
        };
        store.save_settings(&saved).unwrap();
        // the old folder still exists: the saved path is still valid
        rebase_settings(&store, base.path(), &default);
        assert_eq!(store.settings(&default).unwrap(), saved);
        assert_eq!(migrate_data_dir_in(base.path()), DirMove::Moved);
        rebase_settings(&store, base.path(), &default);
        let s = store.settings(&default).unwrap();
        assert_eq!(s.download_dir, base.path().join("nit").join("mirrors"));
        assert_eq!(s.clone_root, Some(PathBuf::from(r"D:\code")));
    }

    #[test]
    fn credentials_move_from_the_old_service_to_the_new_one() {
        let (new, old) = (MemorySecrets::new(), MemorySecrets::new());
        old.set(KEY_BB_EMAIL, EMAIL).unwrap();
        old.set(KEY_BB_TOKEN, TOKEN).unwrap();
        assert_eq!(migrate_credentials(&new, &old), CredMove::Moved);
        assert_eq!(new.get(KEY_BB_EMAIL).unwrap().as_deref(), Some(EMAIL));
        assert_eq!(new.get(KEY_BB_TOKEN).unwrap().as_deref(), Some(TOKEN));
        assert_eq!(old.get(KEY_BB_EMAIL).unwrap(), None);
        assert_eq!(old.get(KEY_BB_TOKEN).unwrap(), None);
        // the next start has nothing to do
        assert_eq!(migrate_credentials(&new, &old), CredMove::Nothing);
    }

    #[test]
    fn credentials_under_the_new_name_are_never_overwritten() {
        let (new, old) = (MemorySecrets::new(), MemorySecrets::new());
        new.set(KEY_BB_EMAIL, "new@example.com").unwrap();
        old.set(KEY_BB_EMAIL, EMAIL).unwrap();
        old.set(KEY_BB_TOKEN, TOKEN).unwrap();
        assert_eq!(migrate_credentials(&new, &old), CredMove::Nothing);
        assert_eq!(
            new.get(KEY_BB_EMAIL).unwrap().as_deref(),
            Some("new@example.com")
        );
        assert_eq!(new.get(KEY_BB_TOKEN).unwrap(), None);
        assert_eq!(old.get(KEY_BB_TOKEN).unwrap().as_deref(), Some(TOKEN));
    }

    #[test]
    fn an_incomplete_old_entry_is_not_moved() {
        let (new, old) = (MemorySecrets::new(), MemorySecrets::new());
        old.set(KEY_BB_EMAIL, EMAIL).unwrap();
        assert_eq!(migrate_credentials(&new, &old), CredMove::Nothing);
        assert_eq!(new.get(KEY_BB_EMAIL).unwrap(), None);
        assert_eq!(old.get(KEY_BB_EMAIL).unwrap().as_deref(), Some(EMAIL));
    }

    /// Fails every write of the token, or every read.
    struct Broken {
        inner: MemorySecrets,
        reads: bool,
    }

    impl SecretStore for Broken {
        fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
            if self.reads {
                return Err(SecretError::Unavailable);
            }
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            if key == KEY_BB_TOKEN {
                return Err(SecretError::TooLong);
            }
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<(), SecretError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn a_failed_copy_keeps_the_old_entries_and_leaves_no_half_entry() {
        let old = MemorySecrets::new();
        old.set(KEY_BB_EMAIL, EMAIL).unwrap();
        old.set(KEY_BB_TOKEN, TOKEN).unwrap();
        let new = Broken {
            inner: MemorySecrets::new(),
            reads: false,
        };
        let out = migrate_credentials(&new, &old);
        assert_eq!(out, CredMove::Failed);
        assert!(!format!("{out:?}").contains(TOKEN));
        assert_eq!(new.get(KEY_BB_EMAIL).unwrap(), None, "no half entry");
        assert_eq!(old.get(KEY_BB_EMAIL).unwrap().as_deref(), Some(EMAIL));
        assert_eq!(old.get(KEY_BB_TOKEN).unwrap().as_deref(), Some(TOKEN));
    }

    #[test]
    fn an_unreadable_new_store_is_left_alone() {
        let old = MemorySecrets::new();
        old.set(KEY_BB_EMAIL, EMAIL).unwrap();
        old.set(KEY_BB_TOKEN, TOKEN).unwrap();
        let new = Broken {
            inner: MemorySecrets::new(),
            reads: true,
        };
        assert_eq!(migrate_credentials(&new, &old), CredMove::Nothing);
        assert_eq!(old.get(KEY_BB_TOKEN).unwrap().as_deref(), Some(TOKEN));
    }

    #[test]
    fn the_new_environment_variable_wins_and_the_old_one_is_the_fallback() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let both = env(&[("NIT_TOKEN", "new"), ("PRVIEW_TOKEN", "old")]);
        assert_eq!(
            env_or_legacy(&both, "NIT_TOKEN", "PRVIEW_TOKEN").as_deref(),
            Some("new")
        );
        let old_only = env(&[("PRVIEW_TOKEN", "old")]);
        assert_eq!(
            env_or_legacy(&old_only, "NIT_TOKEN", "PRVIEW_TOKEN").as_deref(),
            Some("old")
        );
        let blank_new = env(&[("NIT_TOKEN", " "), ("PRVIEW_TOKEN", "old")]);
        assert_eq!(
            env_or_legacy(&blank_new, "NIT_TOKEN", "PRVIEW_TOKEN").as_deref(),
            Some("old")
        );
        assert_eq!(env_or_legacy(&env(&[]), "NIT_TOKEN", "PRVIEW_TOKEN"), None);
    }
}
