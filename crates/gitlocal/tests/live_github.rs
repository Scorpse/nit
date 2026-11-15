//! Opt-in live checks of git over HTTPS against GitHub (spec 7.2, plan Task 13). Never run by
//! default (`#[ignore]`); they need the network, and the authenticated ones a token:
//!
//! ```text
//! set GITHUB_TOKEN=<PAT with Contents: read on the repo>   (never printed)
//! set NIT_GITHUB_LIVE_REPO=<owner>/<repo>                  (private repo proves auth)
//! set NIT_GITHUB_LIVE_PR=<number>                          (optional, for refs/pull)
//! set NIT_GITHUB_LIVE_GIT_HOST=<host>                      (optional, GHES; default github.com)
//! cargo test -p gitlocal --test live_github -- --ignored --nocapture
//! ```
//!
//! The token only ever goes through `gitlocal`'s own auth path (an env-injected,
//! host-scoped `http.extraheader`), exactly as the app fetches.

use gitlocal::{ls_remote, Git, GitAuth, GITHUB_GIT_USER};

fn host() -> String {
    std::env::var("NIT_GITHUB_LIVE_GIT_HOST").unwrap_or_else(|_| "github.com".into())
}

/// Token and repo from the environment, or None (the test then says what is missing).
fn live() -> Option<(String, String)> {
    let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
    let repo = std::env::var("NIT_GITHUB_LIVE_REPO")
        .ok()
        .filter(|r| r.contains('/'));
    match (token, repo) {
        (Some(t), Some(r)) => Some((t, r)),
        _ => {
            eprintln!("skipped: set GITHUB_TOKEN and NIT_GITHUB_LIVE_REPO");
            None
        }
    }
}

/// No credentials at all: the network path and git itself work for a public repository.
#[test]
#[ignore]
fn ls_remote_public_repo_without_credentials() {
    let git = Git::locate().unwrap();
    let dir = std::env::temp_dir();
    let out = git
        .run(
            &dir,
            &["ls-remote", "https://github.com/git/git.git", "HEAD"],
            None,
        )
        .unwrap();
    assert!(String::from_utf8_lossy(&out).contains("HEAD"));
}

/// The pre-sent `Authorization: Basic base64(x-access-token:<PAT>)` header is accepted
/// (spec 7.2 UNVERIFIED item). Use a private repo: a public one would answer without auth.
#[test]
#[ignore]
fn ls_remote_with_extraheader_basic_x_access_token() {
    let Some((token, repo)) = live() else { return };
    let git = Git::locate().unwrap();
    let auth = GitAuth::new(GITHUB_GIT_USER, &token, &host());
    let url = format!("https://{}/{repo}.git", host());
    match ls_remote(&git, &url, &auth, None) {
        Ok(()) => eprintln!(
            "PASS: extraheader Basic x-access-token accepted by {}",
            host()
        ),
        // the error text is scrubbed by gitlocal; it never contains the token
        Err(e) => panic!("FAIL: {e}"),
    }
}

/// `refs/pull/<n>/head` is fetchable with the same credentials (spec 7.1).
#[test]
#[ignore]
fn fetch_refs_pull_head() {
    let Some((token, repo)) = live() else { return };
    let Some(pr) = std::env::var("NIT_GITHUB_LIVE_PR")
        .ok()
        .and_then(|p| p.parse::<u64>().ok())
    else {
        eprintln!("skipped: set NIT_GITHUB_LIVE_PR");
        return;
    };
    let git = Git::locate().unwrap();
    let auth = GitAuth::new(GITHUB_GIT_USER, &token, &host());
    let url = format!("https://{}/{repo}.git", host());
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q", "--bare"], None)
        .unwrap();
    let refspec = format!("+refs/pull/{pr}/head:refs/nit/test/pr-{pr}/src");
    git.run(
        dir.path(),
        &["fetch", "--no-tags", "--depth=1", &url, &refspec],
        Some(&auth),
    )
    .unwrap_or_else(|e| panic!("FAIL: {e}"));
    let sha = git
        .run(
            dir.path(),
            &["rev-parse", &format!("refs/nit/test/pr-{pr}/src")],
            None,
        )
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&sha).trim().len(), 40);
    eprintln!("PASS: refs/pull/{pr}/head fetched");
}
