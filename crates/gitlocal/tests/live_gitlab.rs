//! Opt-in live checks of git over HTTPS against GitLab (spec 7.2, plan Task 22). Never run by
//! default (`#[ignore]`); they need the network and a token:
//!
//! ```text
//! set NIT_LIVE_GITLAB_URL=https://gitlab.com          (or a self-managed web address,
//!                                                       relative URL roots included;
//!                                                       http:// on a private address)
//! set NIT_LIVE_GITLAB_TOKEN=<PAT: api, or read_api + read_repository>   (never printed)
//! set NIT_LIVE_GITLAB_PROJECT=<group/sub/project>     (private project proves auth)
//! set NIT_LIVE_GITLAB_MR=<iid>                        (optional, for refs/merge-requests)
//! cargo test -p gitlocal --test live_gitlab -- --ignored --nocapture
//! ```
//!
//! Run once against gitlab.com and once against a self-managed host (with an internal CA,
//! if available). The token only ever goes through `gitlocal`'s own auth path (an
//! env-injected, host-scoped `http.extraheader`, user `oauth2`), exactly as the app fetches.

use gitlocal::{ls_remote, Git, GitAuth, GITLAB_GIT_USER};

/// `host[/prefix]` of the web address (https, or http on a private address).
fn host_of(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None if prcore::origin::is_private_http_url(url) => url.strip_prefix("http://")?,
        None => return None,
    };
    (!rest.is_empty() && !rest.contains(['?', '#', '@'])).then(|| rest.to_ascii_lowercase())
}

/// The account's git credentials for `host` and the clone URL of `project`: plain http
/// (scoped to exactly that origin) when the web address is http, else https.
fn auth_and_url(host: &str, token: &str, project: &str) -> (GitAuth, String) {
    let http = std::env::var("NIT_LIVE_GITLAB_URL")
        .is_ok_and(|u| u.trim().to_ascii_lowercase().starts_with("http://"));
    let auth = if http {
        GitAuth::cleartext(GITLAB_GIT_USER, token, host).expect("a private http address")
    } else {
        GitAuth::new(GITLAB_GIT_USER, token, host)
    };
    let url = format!("{}://{host}/{project}.git", auth.scheme());
    (auth, url)
}

/// Host (with any path prefix), token and project from the environment, or None (the test
/// then says what is missing).
fn live() -> Option<(String, String, String)> {
    let host = std::env::var("NIT_LIVE_GITLAB_URL")
        .ok()
        .and_then(|u| host_of(&u));
    let token = std::env::var("NIT_LIVE_GITLAB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let project = std::env::var("NIT_LIVE_GITLAB_PROJECT")
        .ok()
        .filter(|p| p.contains('/'));
    match (host, token, project) {
        (Some(h), Some(t), Some(p)) => Some((h, t, p)),
        _ => {
            eprintln!(
                "skipped: set NIT_LIVE_GITLAB_URL (https, or http on a private address), NIT_LIVE_GITLAB_TOKEN and NIT_LIVE_GITLAB_PROJECT"
            );
            None
        }
    }
}

/// The pre-sent `Authorization: Basic base64(oauth2:<PAT>)` header is accepted (spec 7.2
/// UNVERIFIED item). Use a private project: a public one would answer without auth.
#[test]
#[ignore]
fn ls_remote_with_extraheader_basic_oauth2() {
    let Some((host, token, project)) = live() else {
        return;
    };
    let git = Git::locate().unwrap();
    let (auth, url) = auth_and_url(&host, &token, &project);
    match ls_remote(&git, &url, &auth, None) {
        Ok(()) => eprintln!("PASS: extraheader Basic oauth2 accepted by {host}"),
        // the error text is scrubbed by gitlocal; it never contains the token
        Err(e) => panic!("FAIL: {e}"),
    }
}

/// `refs/merge-requests/<iid>/head` is fetchable with the same credentials (spec 7.1).
#[test]
#[ignore]
fn fetch_refs_merge_requests_head() {
    let Some((host, token, project)) = live() else {
        return;
    };
    let Some(iid) = std::env::var("NIT_LIVE_GITLAB_MR")
        .ok()
        .and_then(|p| p.parse::<u64>().ok())
    else {
        eprintln!("skipped: set NIT_LIVE_GITLAB_MR");
        return;
    };
    let git = Git::locate().unwrap();
    let (auth, url) = auth_and_url(&host, &token, &project);
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q", "--bare"], None)
        .unwrap();
    let refspec = format!("+refs/merge-requests/{iid}/head:refs/nit/test/pr-{iid}/src");
    git.run(
        dir.path(),
        &["fetch", "--no-tags", "--depth=1", &url, &refspec],
        Some(&auth),
    )
    .unwrap_or_else(|e| panic!("FAIL: {e}"));
    let sha = git
        .run(
            dir.path(),
            &["rev-parse", &format!("refs/nit/test/pr-{iid}/src")],
            None,
        )
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&sha).trim().len(), 40);
    eprintln!("PASS: refs/merge-requests/{iid}/head fetched");
}

#[test]
fn host_of_keeps_a_relative_url_root_and_needs_https() {
    assert_eq!(
        host_of("https://Git.Corp.Example/gitlab/").as_deref(),
        Some("git.corp.example/gitlab")
    );
    assert_eq!(host_of("https://gitlab.com").as_deref(), Some("gitlab.com"));
    assert_eq!(host_of("http://gitlab.com"), None);
    assert_eq!(host_of("http://192.168.1.1.evil.com"), None);
    assert_eq!(
        host_of("http://192.168.1.20:8080/").as_deref(),
        Some("192.168.1.20:8080")
    );
    assert_eq!(host_of("https://u@gitlab.com"), None);
}
