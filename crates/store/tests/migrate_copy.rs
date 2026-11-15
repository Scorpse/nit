//! Opt-in check of the v7 upgrade on a COPY of a real cache (P0 exit check):
//!
//! ```text
//! copy cache.sqlite out of the data folder first (Windows %LOCALAPPDATA%\nit, macOS
//! ~/Library/Application Support/nit, Linux ~/.local/share/nit), then
//! set NIT_MIGRATION_COPY=<path of the copy>
//! cargo test -p store --test migrate_copy -- --ignored --nocapture
//! ```
//!
//! Refuses any file inside a data folder (the live cache is never opened). Prints row
//! counts before and after `Store::open` (the app's migration path) and checks that nothing
//! the user owns was lost: drafts, viewed marks, the selection and the cached PRs.

use std::path::PathBuf;

use prcore::model::AccountId;
use rusqlite::Connection;
use store::Store;

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

/// Rows per table, read with plain SQL (works on v6 and v7).
fn counts(c: &Connection) -> Vec<(&'static str, i64)> {
    [
        ("prs", "SELECT COUNT(*) FROM prs"),
        ("diffstats", "SELECT COUNT(*) FROM diffstats"),
        ("repos", "SELECT COUNT(*) FROM repos"),
        ("repo_errors", "SELECT COUNT(*) FROM repo_errors"),
        ("viewed", "SELECT COUNT(*) FROM viewed"),
        ("drafts", "SELECT COUNT(*) FROM drafts"),
        ("repo_choices", "SELECT COUNT(*) FROM repo_choices"),
        (
            "selected",
            "SELECT COUNT(*) FROM repo_choices WHERE selected = 1",
        ),
        ("meta", "SELECT COUNT(*) FROM meta"),
    ]
    .into_iter()
    .map(|(name, sql)| (name, count(c, sql)))
    .collect()
}

#[test]
#[ignore = "needs NIT_MIGRATION_COPY: a copy of a real cache"]
fn migrate_a_copy_of_a_real_cache() {
    let path = PathBuf::from(std::env::var("NIT_MIGRATION_COPY").expect("NIT_MIGRATION_COPY"));
    // the data folder of every platform (see `app::brand::data_dir`)
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let live: Vec<PathBuf> = [
        var("LOCALAPPDATA"),
        var("XDG_DATA_HOME"),
        var("HOME").map(|h| h.join(".local").join("share")),
        var("HOME").map(|h| h.join("Library").join("Application Support")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|base| base.join("nit").canonicalize().ok())
    .collect();
    let here = path.canonicalize().expect("the copy exists");
    assert!(
        live.iter().all(|d| !here.starts_with(d)),
        "refusing to open a file inside the live data folder"
    );
    let before = {
        let c = Connection::open(&here).unwrap();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        println!("before: user_version {v}");
        counts(&c)
    };
    let store = Store::open(&here).expect("the upgrade succeeds");
    let snap = store.cache_snapshot().unwrap();
    let accounts = store.accounts().unwrap();
    let choices = store.repo_choices().unwrap();
    drop(store);
    let c = Connection::open(&here).unwrap();
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    let after = counts(&c);
    println!("after:  user_version {v}");
    for ((name, b), (_, a)) in before.iter().zip(&after) {
        println!("  {name:<13} {b:>5} -> {a:>5}");
    }
    println!(
        "accounts: {:?}",
        accounts
            .iter()
            .map(|a| (a.id.as_str(), a.forge.as_str(), a.git_host.as_str()))
            .collect::<Vec<_>>()
    );
    println!(
        "inbox snapshot: {} PRs of {} selected repositories ({} known), {} errors",
        snap.prs.len(),
        snap.repo_count,
        snap.known_repos,
        snap.errors.len()
    );
    let foreign = count(
        &c,
        "SELECT COUNT(*) FROM repo_choices WHERE repo_key NOT LIKE 'bitbucket.org/%'",
    );
    println!("choices without a host-qualified key: {foreign}");
    assert_eq!(v, 7);
    assert_eq!(before, after, "no row lost or added");
    assert_eq!(foreign, 0);
    assert!(choices
        .iter()
        .all(|ch| ch.account == AccountId::default_bitbucket()));
    assert!(snap
        .prs
        .iter()
        .all(|(p, _)| p.account == AccountId::default_bitbucket()));
    // a second open does not migrate again
    let again = Store::open(&here).unwrap();
    assert_eq!(again.repo_choices().unwrap(), choices);
    // the first sign-in after the upgrade, as the same user: nothing is cleared
    if let Some(user) = again.get_meta("account_id").unwrap() {
        let wiped = again
            .adopt_user(&AccountId::default_bitbucket(), &user, "same user")
            .unwrap();
        let snap_after = again.cache_snapshot().unwrap();
        println!(
            "sign-in as the recorded user: cleared {wiped}, inbox {} PRs",
            snap_after.prs.len()
        );
        assert!(!wiped);
        assert_eq!(snap_after.prs.len(), snap.prs.len());
    }
}
