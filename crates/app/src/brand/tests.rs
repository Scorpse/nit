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
