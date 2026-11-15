//! No token in the data folder (spec 12, P5 exit; plan Task 30). Every file under a folder
//! (SQLite with its WAL, logs, reports) is searched for each token in every form it could be
//! written in: the git header forms of `gitlocal::secret_forms`, plus the plain base64 of
//! the token and of Bitbucket's API `email:token`. Failures name the file and the form
//! number, never a value.
//!
//! `no_token_in_data_dir_of_a_fake_session` runs by default on a temp folder written by a
//! fake session with three in-memory accounts; `no_token_in_data_dir` (ignored, live) scans
//! the real data folder (`brand::data_dir`) after a real session, with the tokens given in
//! env.

use std::path::{Path, PathBuf};

use base64::Engine;

use super::*;
use store::MemorySecrets;

/// One credential as the app sends it: the git user (and, for Bitbucket, the email) it goes
/// with.
struct Secret {
    git_user: &'static str,
    email: Option<String>,
    token: String,
}

/// Every textual form of `s` to look for (longest first).
fn forms(s: &Secret) -> Vec<String> {
    let auth = GitAuth::new(s.git_user, &s.token, "example.org");
    let mut out = gitlocal::secret_forms(&auth);
    let mut b64 = |raw: &str| {
        for e in [
            base64::engine::general_purpose::STANDARD,
            base64::engine::general_purpose::URL_SAFE,
        ] {
            let v = e.encode(raw);
            out.push(v.trim_end_matches('=').to_string());
            out.push(v);
        }
    };
    b64(&s.token);
    if let Some(email) = &s.email {
        b64(&format!("{email}:{}", s.token));
    }
    out.retain(|f| f.len() >= 8);
    out.sort_by_key(|f| std::cmp::Reverse(f.len()));
    out.dedup();
    out
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `(file, secret index, form index)` of every hit; empty when the folder is clean.
fn leaks(dir: &Path, secrets: &[Secret]) -> Vec<(PathBuf, usize, usize)> {
    let mut files = Vec::new();
    files_under(dir, &mut files);
    let mut hits = Vec::new();
    for f in files {
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        for (si, s) in secrets.iter().enumerate() {
            for (fi, form) in forms(s).iter().enumerate() {
                let needle = form.as_bytes();
                if bytes.windows(needle.len()).any(|w| w == needle) {
                    hits.push((f.clone(), si, fi));
                }
            }
        }
    }
    hits
}

const BB_TOKEN: &str = "ATATT3xFfGF0-fake-bitbucket-token-0123456789";
const GH_TOKEN: &str = "github_pat_11FAKE0000_fakeGitHubTokenValue0123456789";
const GL_TOKEN: &str = "glpat-FakeGitLabToken-0123456789";

/// A session with a Bitbucket, a GitHub and a GitLab account, all in memory but the data
/// folder: sign-ins as the add flows store them, a synced repository with PRs, diffstats
/// and an error per account, drafts, labels, an expiry date, provider `Debug` and error text
/// in a log, a crash-log entry and a token-check report.
fn fake_session(dir: &Path) -> Vec<Secret> {
    let secrets = MemorySecrets::new();
    let store = Mutex::new(Store::open(&dir.join("cache.sqlite")).unwrap());
    let user = |id: &str, login: &str| UserRef {
        id: id.into(),
        display_name: format!("{login} name"),
        login: Some(login.into()),
    };
    let (bb, _) = add_account(
        &secrets,
        &store,
        "bo@example.com",
        BB_TOKEN,
        &user("{bo}", "bo"),
    )
    .unwrap();
    let gh_bases = providers::github::bases_for("https://github.com").unwrap();
    let (gh, _) = add_github(&secrets, &store, &gh_bases, GH_TOKEN, &user("42", "mona")).unwrap();
    let gl_bases = providers::gitlab::bases_for("https://git.corp.example/gitlab").unwrap();
    let expires = chrono::NaiveDate::from_ymd_opt(2027, 1, 31);
    let (gl, _) = add_gitlab(
        &secrets,
        &store,
        &gl_bases,
        GL_TOKEN,
        &user("17", "alex"),
        expires,
    )
    .unwrap();
    let template = crate::demo::demo_data().1[0].clone();
    let mut log = String::new();
    for (id, host, path) in [
        (&bb, prcore::model::BITBUCKET_HOST, "acme/api"),
        (&gh, "github.com", "octo/web"),
        (&gl, "git.corp.example/gitlab", "grp/sub/proj"),
    ] {
        let repo = RepoRef::parse(host, path).unwrap();
        let st = lock_ok(&store);
        st.record_discovery_for(
            id,
            &[prcore::model::RepoInfo::from(repo.clone())],
            &[],
            None,
        )
        .unwrap();
        st.set_selection_for(id, std::slice::from_ref(&repo))
            .unwrap();
        let mut p = template.clone();
        p.repo = repo.clone();
        p.source_repo = repo.clone();
        p.account = id.clone();
        let stat = DiffStat {
            files: 1,
            lines_added: 2,
            lines_removed: 1,
            max_file_lines: 2,
        };
        let error = ProviderError::RateLimited.category();
        st.apply_repo_sync_for(
            id,
            &repo,
            Some(&[p]),
            &[(template.id, "c1".into(), stat)],
            Some(error.as_str()),
        )
        .unwrap();
        st.save_draft(&prcore::draft::Draft {
            id: 0,
            repo: repo.clone(),
            pr_id: template.id,
            kind: prcore::draft::DraftKind::Pr,
            path: None,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text: "looks good".into(),
            source_commit: "c1".into(),
            comment_type: None,
        })
        .unwrap();
        set_account_label(&st, id, &format!("label of {path}")).unwrap();
        drop(st);
        record_account_error(&store, id, &github_signin_error(&ProviderError::Auth));
        // what a log line could carry: the account, its git auth and provider, error texts
        let account = lock_ok(&store)
            .accounts()
            .unwrap()
            .into_iter()
            .find(|a| a.id == *id)
            .unwrap();
        log.push_str(&format!(
            "{account:?}\n{:?}\n",
            git_auth(&secrets, &account)
        ));
        let p = build_provider(&account, &secrets).unwrap();
        log.push_str(&format!(
            "{:?} {}\n",
            p.capabilities(),
            ProviderError::Network(CACHE_READ_FAILED.into()).user_message()
        ));
    }
    log.push_str(&format!(
        "{:?}\n{:?}\n{:?}\n",
        GitHubProvider::new(
            &gh_bases.api_base,
            &gh_bases.graphql_url,
            &gh_bases.git_host,
            GH_TOKEN
        ),
        GitLabProvider::new(&gl_bases.api_base, &gl_bases.git_host, GL_TOKEN),
        BbClient::new("https://api.bitbucket.org/2.0", "bo@example.com", BB_TOKEN),
    ));
    std::fs::write(dir.join("nit.log"), log).unwrap();
    crate::crashlog::append_rotated(
        &dir.join("crash.log"),
        &crate::crashlog::format_note(
            "2026-11-14 10:00:00",
            "sync",
            &crate::crashlog::scrub(&format!("request failed: Bearer {GH_TOKEN}")),
        ),
        1 << 20,
    )
    .unwrap();
    // the report names the account; render scrubs every token it is given
    let report = crate::checktoken::Report {
        account: Some(format!("GitHub github.com ({GH_TOKEN})")),
        ..Default::default()
    };
    std::fs::write(
        dir.join("token-check.txt"),
        report.render(&[BB_TOKEN, GH_TOKEN, GL_TOKEN]),
    )
    .unwrap();
    // everything written reaches the main file (the WAL is scanned as well when present)
    lock_ok(&store).checkpoint().unwrap();
    drop(store);
    vec![
        Secret {
            git_user: gitlocal::BITBUCKET_GIT_USER,
            email: Some("bo@example.com".into()),
            token: BB_TOKEN.into(),
        },
        Secret {
            git_user: gitlocal::GITHUB_GIT_USER,
            email: None,
            token: GH_TOKEN.into(),
        },
        Secret {
            git_user: gitlocal::GITLAB_GIT_USER,
            email: None,
            token: GL_TOKEN.into(),
        },
    ]
}

#[test]
fn no_token_in_data_dir_of_a_fake_session() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = fake_session(dir.path());
    let mut files = Vec::new();
    files_under(dir.path(), &mut files);
    let names: Vec<String> = files
        .iter()
        .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    for wanted in ["cache.sqlite", "nit.log", "crash.log", "token-check.txt"] {
        assert!(names.iter().any(|n| n == wanted), "{wanted} in {names:?}");
    }
    let hits = leaks(dir.path(), &secrets);
    assert!(
        hits.is_empty(),
        "token forms found (file, token #, form #): {hits:?}"
    );
}

/// The scan is not vacuous: each kind of form it looks for is found where it is written.
#[test]
fn the_scan_finds_every_form_it_looks_for() {
    let s = Secret {
        git_user: gitlocal::BITBUCKET_GIT_USER,
        email: Some("bo@example.com".into()),
        token: BB_TOKEN.into(),
    };
    let all = forms(&s);
    assert!(all.len() >= 6, "{}", all.len());
    for (i, form) in all.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs").join("x.log"), format!("a {form} b")).unwrap();
        let hits = leaks(dir.path(), std::slice::from_ref(&s));
        assert!(hits.iter().any(|h| h.2 == i), "form #{i} not found");
    }
}

/// The real data folder after a real session with the given accounts: run with
/// `cargo test -p app -- --ignored no_token_in_data_dir` and at least one of
/// `NIT_LIVE_GITHUB_TOKEN` / `NIT_GITHUB_TOKEN`, `NIT_LIVE_GITLAB_TOKEN` /
/// `NIT_GITLAB_TOKEN`, `NIT_TOKEN` (with `NIT_EMAIL`, Bitbucket) set to the tokens that
/// session used. Reads the env only; prints file names and form numbers, never a value.
#[test]
#[ignore = "live: scans the data folder after a real session; tokens from env"]
fn no_token_in_data_dir() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut secrets = Vec::new();
    for (var, user) in [
        ("NIT_LIVE_GITHUB_TOKEN", gitlocal::GITHUB_GIT_USER),
        ("NIT_GITHUB_TOKEN", gitlocal::GITHUB_GIT_USER),
        ("NIT_LIVE_GITLAB_TOKEN", gitlocal::GITLAB_GIT_USER),
        ("NIT_GITLAB_TOKEN", gitlocal::GITLAB_GIT_USER),
    ] {
        if let Some(token) = env(var) {
            secrets.push(Secret {
                git_user: user,
                email: None,
                token,
            });
        }
    }
    if let Some(token) = env("NIT_TOKEN") {
        secrets.push(Secret {
            git_user: gitlocal::BITBUCKET_GIT_USER,
            email: env("NIT_EMAIL"),
            token,
        });
    }
    assert!(
        !secrets.is_empty(),
        "set NIT_LIVE_GITHUB_TOKEN, NIT_LIVE_GITLAB_TOKEN and/or NIT_TOKEN (+ NIT_EMAIL)"
    );
    let dir = crate::brand::data_dir().expect(crate::brand::DATA_DIR_UNSET);
    assert!(dir.is_dir(), "no data folder at {}", dir.display());
    let hits = leaks(&dir, &secrets);
    assert!(
        hits.is_empty(),
        "token forms found (file, token #, form #): {hits:?}"
    );
}
